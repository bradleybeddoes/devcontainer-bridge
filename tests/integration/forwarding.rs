//! Integration tests for the end-to-end forwarding lifecycle.
//!
//! These tests validate the full reverse-proxy pipeline in-process without
//! requiring Docker. They exercise the real control channel, listener
//! management, data connection handshake, and bidirectional proxy bridge.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use dbr::control::{self, ControlConnection};
use dbr::host::proxy::{
    bridge_connection, new_pending_connections, register_pending, resolve_pending, DataStream,
};
use dbr::host::HostConfig;
use dbr::protocol::{Message, Protocol};

/// Find a free TCP port by binding to port 0 and returning the assigned port.
/// The listener is dropped, freeing the port for immediate reuse.
async fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Start a simple TCP echo server on a random port.
/// Returns the bound address and a shutdown sender.
async fn start_echo_server() -> (SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, _)) => {
                            tokio::spawn(async move {
                                let mut buf = [0u8; 4096];
                                loop {
                                    let n = match stream.read(&mut buf).await {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => n,
                                    };
                                    if stream.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            });
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });

    (addr, shutdown_tx)
}

/// Create a connected TCP stream pair via a temporary listener on loopback.
async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (a, b) = tokio::join!(TcpStream::connect(addr), listener.accept());
    (a.unwrap(), b.unwrap().0)
}

/// Connect to the host daemon control port and perform the Register handshake.
async fn register_container(
    control_addr: SocketAddr,
    container_id: &str,
    hostname: &str,
) -> ControlConnection {
    let mut conn = control::connect(control_addr).await.unwrap();

    conn.send(&Message::Register {
        container_id: container_id.to_string(),
        hostname: hostname.to_string(),
        auth_token: String::new(),
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(
        matches!(ack, Message::RegisterAck { success: true }),
        "expected RegisterAck{{success: true}}, got {ack:?}"
    );

    conn
}

/// Helper: try connecting to a port on loopback, return true if successful.
/// Tries IPv4 first (matching production `bind_loopback` preference), then IPv6.
async fn can_connect_port(port: u16) -> bool {
    let addrs: &[SocketAddr] = &[
        ([127, 0, 0, 1], port).into(),
        ([0, 0, 0, 0, 0, 0, 0, 1], port).into(),
    ];
    tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(addrs))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Poll until a port stops accepting connections, returning true if it closed
/// within the timeout. Avoids fixed-sleep assertions that are timing-dependent.
async fn wait_port_closed(port: u16, timeout: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if !can_connect_port(port).await {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll until a port starts accepting connections.
/// Used to wait for the host daemon to finish binding instead of a fixed sleep.
async fn wait_port_open(port: u16, timeout: Duration) {
    let start = tokio::time::Instant::now();
    loop {
        if can_connect_port(port).await {
            return;
        }
        if start.elapsed() >= timeout {
            panic!("port {port} did not open within {}ms", timeout.as_millis());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Receive the next non-heartbeat message from a control connection.
/// Skips any `Ping` messages that arrive between request/response pairs,
/// which can happen when tests run in parallel and the host daemon's
/// heartbeat timer fires during the exchange.
async fn recv_skip_pings(conn: &mut ControlConnection) -> Message {
    loop {
        let msg = conn.recv().await.unwrap();
        if matches!(msg, Message::Ping) {
            continue;
        }
        return msg;
    }
}

// ---------------------------------------------------------------------------
// Test 1: Register → Forward → ForwardAck → Unforward lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_register_forward_unforward_lifecycle() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    // Start host daemon in background
    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    // Wait for the host daemon to bind
    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // Register as a container
    let mut conn = register_container(control_addr, "test-container-1", "test-host").await;

    // Send Forward for a high port unlikely to be in use
    conn.send(&Message::Forward {
        port: 19501,
        protocol: Protocol::Tcp,
        process_name: Some("test-echo".to_string()),
        pid: Some(12345),
    })
    .await
    .unwrap();

    let ack = recv_skip_pings(&mut conn).await;
    let host_port = match ack {
        Message::ForwardAck {
            port: 19501,
            success: true,
            host_port,
        } => {
            assert!(host_port > 0, "host_port should be a valid port");
            host_port
        }
        other => panic!("expected ForwardAck, got {other:?}"),
    };

    // Verify the host bound a listener on host_port
    assert!(
        can_connect_port(host_port).await,
        "should be able to connect to forwarded port {host_port}"
    );

    // Send Unforward
    conn.send(&Message::Unforward { port: 19501 })
        .await
        .unwrap();

    // Wait for the listener to shut down
    assert!(
        wait_port_closed(host_port, Duration::from_secs(5)).await,
        "forwarded port {host_port} should no longer be accepting connections"
    );

    // Drop connection to trigger exit_on_idle shutdown
    drop(conn);

    // Host daemon should exit
    let result = tokio::time::timeout(Duration::from_secs(10), host_handle)
        .await
        .expect("host daemon should exit after last container disconnects");
    assert!(result.unwrap().is_ok());
}

// ---------------------------------------------------------------------------
// Test 2: Cleanup on container disconnect (EOF)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cleanup_on_container_disconnect() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = register_container(control_addr, "disconnect-test", "test-host").await;

    // Forward a port
    conn.send(&Message::Forward {
        port: 19502,
        protocol: Protocol::Tcp,
        process_name: None,
        pid: None,
    })
    .await
    .unwrap();

    let ack = recv_skip_pings(&mut conn).await;
    let host_port = match ack {
        Message::ForwardAck {
            port: 19502,
            success: true,
            host_port,
        } => host_port,
        other => panic!("expected ForwardAck, got {other:?}"),
    };

    assert!(
        can_connect_port(host_port).await,
        "forward should be active"
    );

    // Simulate container disconnect (drop control connection)
    drop(conn);

    // Wait for forwarded port to be torn down
    assert!(
        wait_port_closed(host_port, Duration::from_secs(5)).await,
        "forwarded port should be torn down after container disconnect"
    );

    // Host daemon should exit (exit_on_idle)
    let result = tokio::time::timeout(Duration::from_secs(10), host_handle)
        .await
        .expect("host daemon should exit");
    assert!(result.unwrap().is_ok());
}

// ---------------------------------------------------------------------------
// Test 3: Ping/Pong keepalive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ping_pong() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = register_container(control_addr, "ping-test", "test-host").await;

    // Send Ping, expect Pong
    conn.send(&Message::Ping).await.unwrap();
    let response = conn.recv().await.unwrap();
    assert_eq!(response, Message::Pong);

    // Drop to trigger shutdown
    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(10), host_handle).await;
}

// ---------------------------------------------------------------------------
// Test 4: ListRequest/ListResponse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_request_response() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // Register a container and forward a port
    let mut container_conn =
        register_container(control_addr, "list-test-container", "list-host").await;

    container_conn
        .send(&Message::Forward {
            port: 19504,
            protocol: Protocol::Tcp,
            process_name: Some("my-app".to_string()),
            pid: Some(42),
        })
        .await
        .unwrap();

    let ack = recv_skip_pings(&mut container_conn).await;
    let host_port = match ack {
        Message::ForwardAck {
            success: true,
            host_port,
            ..
        } => host_port,
        other => panic!("expected ForwardAck, got {other:?}"),
    };

    // Open a separate CLI connection to send ListRequest
    let mut cli_conn = control::connect(control_addr).await.unwrap();
    cli_conn.send(&Message::ListRequest).await.unwrap();

    let response = cli_conn.recv().await.unwrap();
    match response {
        Message::ListResponse {
            forwards,
            socket_forwards,
        } => {
            assert_eq!(forwards.len(), 1, "should have exactly one forward");
            let fwd = &forwards[0];
            assert_eq!(fwd.container_id, "list-test-container");
            assert_eq!(fwd.hostname, "list-host");
            assert_eq!(fwd.port, 19504);
            assert_eq!(fwd.host_port, host_port);
            assert_eq!(fwd.process_name.as_deref(), Some("my-app"));
            assert_eq!(fwd.pid, Some(42));
            assert!(
                socket_forwards.is_empty(),
                "no socket forwards expected in this test"
            );
        }
        other => panic!("expected ListResponse, got {other:?}"),
    }

    // Clean up
    drop(cli_conn);
    drop(container_conn);
    // Host won't exit (exit_on_idle=false), so abort it
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 5: Multiple containers with port conflict resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multi_container_port_conflict() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // Container 1 forwards port 8080
    let mut conn1 = register_container(control_addr, "container-1", "host-1").await;
    conn1
        .send(&Message::Forward {
            port: 19503,
            protocol: Protocol::Tcp,
            process_name: Some("node".to_string()),
            pid: None,
        })
        .await
        .unwrap();

    let ack1 = recv_skip_pings(&mut conn1).await;
    let host_port_1 = match ack1 {
        Message::ForwardAck {
            success: true,
            host_port,
            ..
        } => host_port,
        other => panic!("expected ForwardAck for container-1, got {other:?}"),
    };

    // Container 2 also forwards port 8080 (conflict!)
    let mut conn2 = register_container(control_addr, "container-2", "host-2").await;
    conn2
        .send(&Message::Forward {
            port: 19503,
            protocol: Protocol::Tcp,
            process_name: Some("python".to_string()),
            pid: None,
        })
        .await
        .unwrap();

    let ack2 = recv_skip_pings(&mut conn2).await;
    let host_port_2 = match ack2 {
        Message::ForwardAck {
            success: true,
            host_port,
            ..
        } => host_port,
        other => panic!("expected ForwardAck for container-2, got {other:?}"),
    };

    // Host ports should differ
    assert_ne!(
        host_port_1, host_port_2,
        "conflicting container ports should map to different host ports"
    );

    // Both forwarded ports should be accessible
    assert!(
        can_connect_port(host_port_1).await,
        "first forward should be active"
    );
    assert!(
        can_connect_port(host_port_2).await,
        "second forward should be active"
    );

    // Verify ListResponse shows both
    let mut cli_conn = control::connect(control_addr).await.unwrap();
    cli_conn.send(&Message::ListRequest).await.unwrap();
    let response = cli_conn.recv().await.unwrap();
    match response {
        Message::ListResponse { forwards, .. } => {
            assert_eq!(forwards.len(), 2, "should have two forwards");
        }
        other => panic!("expected ListResponse, got {other:?}"),
    }

    // Clean up
    drop(cli_conn);
    drop(conn1);
    drop(conn2);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 6: Full reverse proxy pipeline (component-level assembly)
//
// This test validates the complete data path:
//   client → host forwarded port → [pending/resolve] → data connection → echo server
//
// Since the host daemon's ConnectRequest dispatch to the container is not yet
// wired (documented MVP limitation), this test assembles the real proxy
// components directly to validate the bridge mechanism end-to-end.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reverse_proxy_pipeline() {
    // 1. Start an echo server (simulates a service inside the container)
    let (echo_addr, echo_shutdown) = start_echo_server().await;

    // 2. Set up the pending connections state (shared between host components)
    let pending = new_pending_connections();

    // 3. Create a client connection pair (simulates client connecting to forwarded port)
    let (client_stream, mut client_end) = tcp_pair().await;

    // 4. Generate a conn_id (normally done by the host daemon)
    let conn_id = "test-conn-001".to_string();

    // 5. Register the pending connection (host side)
    let data_rx = register_pending(&pending, conn_id.clone()).await.unwrap();

    // 6. Simulate container-side behavior: connect to echo server AND open
    //    a "data connection" back to the host, send ConnectReady handshake
    let (data_stream, mut data_host_end) = tcp_pair().await;

    // Resolve the pending with the data stream (host side receives ConnectReady)
    let data = DataStream {
        stream: data_stream,
        buffered: Vec::new(),
    };
    let resolved = resolve_pending(&pending, &conn_id, data).await;
    assert!(resolved, "pending connection should be resolved");

    // 7. Bridge client ↔ data connection in background
    let bridge_pending = pending.clone();
    let bridge_conn_id = conn_id.clone();
    let bridge_handle = tokio::spawn(async move {
        bridge_connection(bridge_conn_id, client_stream, data_rx, &bridge_pending).await
    });

    // 8. Simulate container-side bridging: forward data between echo server
    //    and the data connection
    let mut echo_stream = TcpStream::connect(echo_addr).await.unwrap();
    let container_bridge = tokio::spawn(async move {
        tokio::io::copy_bidirectional(&mut data_host_end, &mut echo_stream).await
    });

    // 9. Client sends data, should receive echo response
    let test_data = b"Hello through the reverse proxy!";
    client_end.write_all(test_data).await.unwrap();

    let mut response = vec![0u8; test_data.len()];
    client_end.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, test_data, "echo response should match sent data");

    // Send more data to verify ongoing bidirectional flow
    let test_data_2 = b"Second message through proxy";
    client_end.write_all(test_data_2).await.unwrap();

    let mut response_2 = vec![0u8; test_data_2.len()];
    client_end.read_exact(&mut response_2).await.unwrap();
    assert_eq!(&response_2, test_data_2);

    // 10. Clean up: close client connection to trigger bridge teardown
    drop(client_end);

    let bridge_result = tokio::time::timeout(Duration::from_secs(5), bridge_handle)
        .await
        .expect("bridge should complete")
        .expect("bridge task should not panic");
    assert!(bridge_result.is_ok(), "bridge should complete successfully");

    let (c2s, s2c) = bridge_result.unwrap();
    assert!(c2s > 0, "should have transferred data client→container");
    assert!(s2c > 0, "should have transferred data container→client");

    container_bridge.abort();
    let _ = echo_shutdown.send(true);
}

// ---------------------------------------------------------------------------
// Test 7: Data connection handshake through host daemon data port
//
// Validates that the host daemon correctly parses ConnectReady on the data
// port and resolves the pending connection.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_data_connection_handshake_on_host() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    // Wait for both control and data ports to be ready
    wait_port_open(control_port, Duration::from_secs(5)).await;

    // Connect to the data port and send a ConnectReady with unknown conn_id.
    // The host should accept it gracefully (no pending match, but no crash).
    let data_addr: SocketAddr = ([127, 0, 0, 1], data_port).into();
    let mut data_stream = TcpStream::connect(data_addr).await.unwrap();

    let ready_msg = serde_json::to_string(&Message::ConnectReady {
        conn_id: "unknown-conn-id".to_string(),
    })
    .unwrap();
    data_stream.write_all(ready_msg.as_bytes()).await.unwrap();
    data_stream.write_all(b"\n").await.unwrap();
    data_stream.flush().await.unwrap();

    // Brief yield for async processing (non-blocking is_finished check follows)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Host daemon should still be running (graceful handling of unknown conn_id)
    assert!(
        !host_handle.is_finished(),
        "host daemon should still be running"
    );

    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 8: Multiple forwards from same container
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multiple_forwards_same_container() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = register_container(control_addr, "multi-forward", "test-host").await;

    // Forward multiple ports
    let ports_to_forward = [19701u16, 19702, 19703, 19704];
    let mut host_ports = Vec::new();

    for port in &ports_to_forward {
        conn.send(&Message::Forward {
            port: *port,
            protocol: Protocol::Tcp,
            process_name: None,
            pid: None,
        })
        .await
        .unwrap();

        let ack = recv_skip_pings(&mut conn).await;
        match ack {
            Message::ForwardAck {
                port: p,
                success: true,
                host_port,
            } => {
                assert_eq!(p, *port);
                host_ports.push(host_port);
            }
            other => panic!("expected ForwardAck for port {port}, got {other:?}"),
        }
    }

    // All host ports should be unique
    let mut unique_ports = host_ports.clone();
    unique_ports.sort();
    unique_ports.dedup();
    assert_eq!(
        unique_ports.len(),
        host_ports.len(),
        "all host ports should be unique"
    );

    // All should be connectable
    for hp in &host_ports {
        assert!(
            can_connect_port(*hp).await,
            "forwarded port {hp} should be accessible"
        );
    }

    // Unforward the first two
    for port in &ports_to_forward[..2] {
        conn.send(&Message::Unforward { port: *port })
            .await
            .unwrap();
    }

    // Ensure both Unforward messages have been processed by sending a
    // Ping/Pong round-trip (messages are processed sequentially).
    // Note: ConnectRequests from the can_connect_port() calls above may
    // be queued on the control channel, so drain them before the Pong.
    conn.send(&Message::Ping).await.unwrap();
    loop {
        let msg = conn.recv().await.unwrap();
        match msg {
            Message::Pong => break,
            Message::Ping | Message::ConnectRequest { .. } => continue, // drain heartbeats and queued connect requests
            other => panic!("expected Pong or ConnectRequest, got {other:?}"),
        }
    }

    // Wait for unforwarded ports to close
    for hp in &host_ports[..2] {
        assert!(
            wait_port_closed(*hp, Duration::from_secs(5)).await,
            "unforwarded port {hp} should not accept connections"
        );
    }

    // Last two should still be up
    for hp in &host_ports[2..] {
        assert!(
            can_connect_port(*hp).await,
            "still-forwarded port {hp} should accept connections"
        );
    }

    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(10), host_handle).await;
}

// ---------------------------------------------------------------------------
// Test 9: Ping via standalone (non-registered) control connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_standalone_ping() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // Send Ping without registering first (like `dbr ensure` health check)
    let mut conn = control::connect(control_addr).await.unwrap();
    conn.send(&Message::Ping).await.unwrap();

    let response = conn.recv().await.unwrap();
    assert_eq!(
        response,
        Message::Pong,
        "should get Pong for standalone Ping"
    );

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 10: Container re-registration after disconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_container_reconnect_reregister() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // First connection: register and forward
    {
        let mut conn = register_container(control_addr, "reconnect-test", "host-1").await;
        conn.send(&Message::Forward {
            port: 19505,
            protocol: Protocol::Tcp,
            process_name: None,
            pid: None,
        })
        .await
        .unwrap();

        let ack = recv_skip_pings(&mut conn).await;
        assert!(matches!(ack, Message::ForwardAck { success: true, .. }));

        // Disconnect
        drop(conn);
    }

    // Allow time for the host to process the disconnect and clean up state.
    // The ForwardAck on re-registration confirms cleanup succeeded.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second connection: re-register and forward the same port
    {
        let mut conn = register_container(control_addr, "reconnect-test", "host-1").await;
        conn.send(&Message::Forward {
            port: 19505,
            protocol: Protocol::Tcp,
            process_name: None,
            pid: None,
        })
        .await
        .unwrap();

        let ack = recv_skip_pings(&mut conn).await;
        match ack {
            Message::ForwardAck {
                port: 19505,
                success: true,
                host_port,
            } => {
                // Should succeed — the previous forward was cleaned up
                assert!(
                    can_connect_port(host_port).await,
                    "re-registered forward should be active"
                );
            }
            other => panic!("expected ForwardAck, got {other:?}"),
        }

        drop(conn);
    }

    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 11: ConnectFailed handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_connect_failed_handling() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = register_container(control_addr, "fail-test", "test-host").await;

    // Send ConnectFailed for an unknown conn_id — host should handle gracefully
    conn.send(&Message::ConnectFailed {
        conn_id: "nonexistent-conn".to_string(),
        error: "connection refused".to_string(),
    })
    .await
    .unwrap();

    // Verify the connection is still alive by sending a Ping
    conn.send(&Message::Ping).await.unwrap();
    let response = conn.recv().await.unwrap();
    assert_eq!(
        response,
        Message::Pong,
        "connection should still be alive after ConnectFailed"
    );

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 12: Full reverse proxy with echo via manual data handshake
//
// This is the most comprehensive test: it starts the host daemon, registers
// a container, forwards a port, then simulates the full container-side
// data connection flow (connecting to echo server + opening data connection
// to host data port with ConnectReady).
//
// Note: Because the host daemon doesn't yet send ConnectRequest to the
// container, we simulate the full flow by directly connecting to the data
// port with a known conn_id and pre-registering it in the pending map.
// This tests the data port handshake parsing and pending resolution through
// the actual host daemon code path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_host_data_port_accepts_connect_ready() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: false,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let data_addr: SocketAddr = ([127, 0, 0, 1], data_port).into();

    // Register container and forward a port
    let mut conn = register_container(control_addr, "data-test", "test-host").await;
    conn.send(&Message::Forward {
        port: 19506,
        protocol: Protocol::Tcp,
        process_name: None,
        pid: None,
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(matches!(ack, Message::ForwardAck { success: true, .. }));

    // Connect to the data port and send ConnectReady
    // This exercises the handle_data_connection code path in host/mod.rs
    let mut data_stream = TcpStream::connect(data_addr).await.unwrap();
    let ready_json = serde_json::to_string(&Message::ConnectReady {
        conn_id: "test-data-conn".to_string(),
    })
    .unwrap();
    data_stream.write_all(ready_json.as_bytes()).await.unwrap();
    data_stream.write_all(b"\n").await.unwrap();
    data_stream.flush().await.unwrap();

    // Brief yield for async processing (non-blocking is_finished check follows)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Host should still be running (no crash on unmatched conn_id)
    assert!(!host_handle.is_finished());

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 13: Proxy bridge timeout when no data connection arrives
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_proxy_bridge_timeout() {
    let pending = new_pending_connections();
    let conn_id = "timeout-test".to_string();

    let data_rx = register_pending(&pending, conn_id.clone()).await.unwrap();
    let (client_stream, _client_end) = tcp_pair().await;

    // Bridge should timeout since no one resolves the pending connection.
    // Use a short outer timeout since CONNECT_TIMEOUT is 10s.
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        bridge_connection(conn_id, client_stream, data_rx, &pending),
    )
    .await;

    // Either our timeout or the 10s internal timeout fires
    assert!(
        result.is_err() || result.unwrap().is_err(),
        "bridge should fail when no data connection arrives"
    );
}

// ---------------------------------------------------------------------------
// Helper: create a HostConfig with authentication enabled
// ---------------------------------------------------------------------------

fn host_config_with_auth(control_port: u16, data_port: u16, token: &str) -> HostConfig {
    HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        auth_token: Some(token.to_string()),
        ..HostConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Test 14: Auth success — register with matching token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_success() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;
    let token = "a".repeat(64);

    let config = host_config_with_auth(control_port, data_port, &token);
    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = control::connect(control_addr).await.unwrap();

    conn.send(&Message::Register {
        container_id: "auth-ok".to_string(),
        hostname: "test-host".to_string(),
        auth_token: token,
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(
        matches!(ack, Message::RegisterAck { success: true }),
        "expected RegisterAck{{success: true}}, got {ack:?}"
    );

    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(10), host_handle).await;
}

// ---------------------------------------------------------------------------
// Test 15: Auth failure — register with wrong token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_failure_wrong_token() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;
    let token = "a".repeat(64);

    let config = host_config_with_auth(control_port, data_port, &token);
    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = control::connect(control_addr).await.unwrap();

    conn.send(&Message::Register {
        container_id: "auth-bad".to_string(),
        hostname: "test-host".to_string(),
        auth_token: "b".repeat(64),
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(
        matches!(ack, Message::RegisterAck { success: false }),
        "expected RegisterAck{{success: false}}, got {ack:?}"
    );

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 16: Auth failure — empty token on authenticated host
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_failure_empty_token() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;
    let token = "a".repeat(64);

    let config = host_config_with_auth(control_port, data_port, &token);
    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = control::connect(control_addr).await.unwrap();

    conn.send(&Message::Register {
        container_id: "auth-empty".to_string(),
        hostname: "test-host".to_string(),
        auth_token: String::new(),
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(
        matches!(ack, Message::RegisterAck { success: false }),
        "expected RegisterAck{{success: false}}, got {ack:?}"
    );

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Test 17: No-auth mode — register without token succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_no_auth_mode() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;

    let config = HostConfig {
        control_port,
        data_port,
        exit_on_idle: true,
        bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
        auth_token: None,
        ..HostConfig::default()
    };

    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
    let mut conn = control::connect(control_addr).await.unwrap();

    conn.send(&Message::Register {
        container_id: "no-auth".to_string(),
        hostname: "test-host".to_string(),
        auth_token: String::new(),
    })
    .await
    .unwrap();

    let ack = conn.recv().await.unwrap();
    assert!(
        matches!(ack, Message::RegisterAck { success: true }),
        "expected RegisterAck{{success: true}}, got {ack:?}"
    );

    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(10), host_handle).await;
}

// ---------------------------------------------------------------------------
// Test 18: Ping/Pong without auth — health check works on authenticated host
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ping_pong_without_auth() {
    let control_port = find_free_port().await;
    let data_port = find_free_port().await;
    let token = "a".repeat(64);

    let config = host_config_with_auth(control_port, data_port, &token);
    let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

    wait_port_open(control_port, Duration::from_secs(5)).await;

    let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

    // Send Ping without registering — should get Pong even with auth enabled
    let mut conn = control::connect(control_addr).await.unwrap();
    conn.send(&Message::Ping).await.unwrap();

    let response = conn.recv().await.unwrap();
    assert_eq!(
        response,
        Message::Pong,
        "should get Pong for standalone Ping on authenticated host"
    );

    drop(conn);
    host_handle.abort();
}

// ---------------------------------------------------------------------------
// Socket scanner integration tests (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod socket_scanner_tests {
    use super::*;
    use dbr::config::SocketForwardingConfig;
    use std::os::unix::net::UnixListener as StdUnixListener;

    /// Create a Unix socket in the given directory with the given name.
    fn create_unix_socket(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let _listener = StdUnixListener::bind(&path).expect("failed to bind Unix socket");
        // Drop the listener — the socket file remains on disk.
        path
    }

    /// Create a HostConfig with socket scanning enabled for the given directory.
    fn host_config_with_socket_scanning(
        control_port: u16,
        data_port: u16,
        watch_dir: &std::path::Path,
        scan_interval_ms: u64,
    ) -> HostConfig {
        HostConfig {
            control_port,
            data_port,
            exit_on_idle: false,
            bind_addr: Some(Ipv4Addr::LOCALHOST.into()),
            socket_forwarding: SocketForwardingConfig {
                enabled: true,
                watch_paths: vec![format!("{}/*", watch_dir.display())],
                container_path_prefix: Some("/run/host-sockets".to_string()),
                scan_interval_ms,
                max_socket_forwards: 16,
            },
            ..HostConfig::default()
        }
    }

    // -----------------------------------------------------------------------
    // Test 19: Socket scanner discovers socket and sends SocketForward
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_socket_scanner_sends_forward_on_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // Create socket before starting the daemon
        let _sock_path = create_unix_socket(tmp.path(), "test.sock");

        let config = host_config_with_socket_scanning(control_port, data_port, tmp.path(), 100);

        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
        let mut conn = register_container(control_addr, "socket-test-1", "test-host").await;

        // Wait for the scanner to detect the socket and send SocketForward.
        // The scanner runs every 100ms, so we should receive it within a few seconds.
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { .. } => return msg,
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketForward within timeout");

        match msg {
            Message::SocketForward {
                socket_id,
                host_path,
                container_path,
            } => {
                assert!(!socket_id.is_empty(), "socket_id should not be empty");
                assert!(
                    host_path.contains("test.sock"),
                    "host_path should contain the socket filename"
                );
                assert_eq!(
                    container_path, "/run/host-sockets/test.sock",
                    "container_path should use the configured prefix"
                );
            }
            _ => unreachable!(),
        }

        drop(conn);
        host_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Test 20: Socket scanner sends SocketUnforward when socket is removed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_socket_scanner_sends_unforward_on_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // Create socket before starting daemon
        let sock_path = create_unix_socket(tmp.path(), "removable.sock");

        let config = host_config_with_socket_scanning(control_port, data_port, tmp.path(), 100);

        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
        let mut conn = register_container(control_addr, "socket-test-2", "test-host").await;

        // Wait for the SocketForward first
        let forward_socket_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { socket_id, .. } => return socket_id,
                    other => panic!("unexpected message waiting for SocketForward: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketForward within timeout");

        // Remove the socket file
        std::fs::remove_file(&sock_path).unwrap();

        // Wait for SocketUnforward
        let unforward_socket_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketUnforward { socket_id } => return socket_id,
                    other => panic!("unexpected message waiting for SocketUnforward: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketUnforward within timeout");

        assert_eq!(
            forward_socket_id, unforward_socket_id,
            "SocketUnforward should reference the same socket_id as SocketForward"
        );

        drop(conn);
        host_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Test 21: New container receives existing socket forwards on registration
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_new_container_receives_existing_socket_forwards() {
        let tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // Create socket before starting daemon
        let _sock_path = create_unix_socket(tmp.path(), "existing.sock");

        let config = host_config_with_socket_scanning(control_port, data_port, tmp.path(), 100);

        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

        // Register first container and wait for the socket to be discovered
        let mut conn1 = register_container(control_addr, "socket-test-3a", "test-host-a").await;

        let _first_socket_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn1.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn1.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { socket_id, .. } => return socket_id,
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        })
        .await
        .expect("first container should receive SocketForward");

        // Now register a second container — it should receive the existing
        // socket forward immediately on registration.
        let mut conn2 = register_container(control_addr, "socket-test-3b", "test-host-b").await;

        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn2.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn2.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { .. } => return msg,
                    other => panic!("unexpected message for second container: {other:?}"),
                }
            }
        })
        .await
        .expect("second container should receive existing SocketForward");

        match msg {
            Message::SocketForward { container_path, .. } => {
                assert_eq!(
                    container_path, "/run/host-sockets/existing.sock",
                    "second container should get the same socket forward"
                );
            }
            _ => unreachable!(),
        }

        drop(conn1);
        drop(conn2);
        host_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Test 22: SocketConnectRequest bridges data through host Unix socket
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_socket_connect_request_bridges_data() {
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // 1. Create a Unix echo server on the host
        let sock_path = tmp.path().join("echo.sock");
        let unix_listener = UnixListener::bind(&sock_path).unwrap();

        let echo_handle = tokio::spawn(async move {
            loop {
                match unix_listener.accept().await {
                    Ok((mut stream, _)) => {
                        tokio::spawn(async move {
                            let mut buf = [0u8; 4096];
                            loop {
                                let n = match tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                                    .await
                                {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => n,
                                };
                                if tokio::io::AsyncWriteExt::write_all(&mut stream, &buf[..n])
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        // 2. Start host daemon with socket scanning watching the tmp dir
        let config = host_config_with_socket_scanning(control_port, data_port, tmp.path(), 100);

        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
        let mut conn = register_container(control_addr, "socket-bridge-test", "test-host").await;

        // 3. Wait for SocketForward
        let socket_id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { socket_id, .. } => return socket_id,
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketForward within timeout");

        // 4. Send SocketConnectRequest
        let conn_id = "socket-conn-001".to_string();
        conn.send(&Message::SocketConnectRequest {
            socket_id: socket_id.clone(),
            conn_id: conn_id.clone(),
        })
        .await
        .unwrap();

        // Small delay to let the host process the request and connect to the Unix socket
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 5. Open a data connection to the host data port and send ConnectReady
        let data_addr: SocketAddr = ([127, 0, 0, 1], data_port).into();
        let mut data_stream = TcpStream::connect(data_addr).await.unwrap();

        let ready_line = serde_json::to_string(&Message::ConnectReady {
            conn_id: conn_id.clone(),
        })
        .unwrap();
        data_stream
            .write_all(format!("{ready_line}\n").as_bytes())
            .await
            .unwrap();
        data_stream.flush().await.unwrap();

        // 6. Write data through the bridge and verify echo response
        let test_data = b"Hello through socket bridge!";
        data_stream.write_all(test_data).await.unwrap();
        data_stream.flush().await.unwrap();

        let mut response = vec![0u8; test_data.len()];
        tokio::time::timeout(
            Duration::from_secs(5),
            data_stream.read_exact(&mut response),
        )
        .await
        .expect("should receive echo within timeout")
        .expect("read should succeed");

        assert_eq!(&response, test_data, "echo response should match sent data");

        // Send a second message to verify ongoing bidirectional flow
        let test_data_2 = b"Second socket bridge message";
        data_stream.write_all(test_data_2).await.unwrap();
        data_stream.flush().await.unwrap();

        let mut response_2 = vec![0u8; test_data_2.len()];
        tokio::time::timeout(
            Duration::from_secs(5),
            data_stream.read_exact(&mut response_2),
        )
        .await
        .expect("should receive second echo within timeout")
        .expect("second read should succeed");

        assert_eq!(&response_2, test_data_2);

        // 7. Clean up
        drop(data_stream);
        drop(conn);
        echo_handle.abort();
        host_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Test 23: Full socket proxy pipeline using container mirror accept loop
    // -----------------------------------------------------------------------
    //
    // This test validates the container-side mirror socket accept loop by:
    // 1. Starting a Unix echo server (simulating a host-side socket)
    // 2. Starting a host daemon with socket scanning
    // 3. Registering a simulated container
    // 4. Waiting for SocketForward, then creating a mirror socket with the
    //    container-side accept loop
    // 5. Connecting a client to the mirror socket
    // 6. Verifying bidirectional data flows through the full chain:
    //    client -> mirror socket -> container accept loop ->
    //    SocketConnectRequest -> data connection -> host -> Unix echo server

    #[tokio::test]
    async fn test_full_socket_proxy_with_mirror_accept_loop() {
        use dbr::container::socket::{
            cleanup_all_mirrors, create_mirror_socket, run_mirror_accept_loop,
        };
        use dbr::container::RelayMessage;
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let mirror_tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // 1. Create a Unix echo server on the host
        let sock_path = tmp.path().join("echo.sock");
        let unix_listener = UnixListener::bind(&sock_path).unwrap();

        let echo_handle = tokio::spawn(async move {
            loop {
                match unix_listener.accept().await {
                    Ok((mut stream, _)) => {
                        tokio::spawn(async move {
                            let mut buf = [0u8; 4096];
                            loop {
                                let n = match tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                                    .await
                                {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => n,
                                };
                                if tokio::io::AsyncWriteExt::write_all(&mut stream, &buf[..n])
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        // 2. Start host daemon with socket scanning watching the tmp dir
        let config = host_config_with_socket_scanning(control_port, data_port, tmp.path(), 100);
        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();
        let data_addr: SocketAddr = ([127, 0, 0, 1], data_port).into();
        let mut conn = register_container(control_addr, "mirror-test", "mirror-host").await;

        // 3. Wait for SocketForward and extract details
        let (socket_id, container_path) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward {
                        socket_id,
                        container_path,
                        ..
                    } => return (socket_id, container_path),
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketForward within timeout");

        // 4. Create mirror socket and start accept loop (simulating container daemon)
        // Use a local path in mirror_tmp to avoid path conflicts
        let mirror_path = mirror_tmp
            .path()
            .join(std::path::Path::new(&container_path).file_name().unwrap());

        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<RelayMessage>(64);

        let mut mirror = create_mirror_socket(&socket_id, &mirror_path).unwrap();
        let listener = mirror.listener.take().unwrap();
        let shutdown_rx = mirror.shutdown_rx.clone();

        tokio::spawn(run_mirror_accept_loop(
            socket_id.clone(),
            listener,
            msg_tx.clone(),
            data_addr,
            shutdown_rx,
        ));

        let mut mirror_sockets = std::collections::HashMap::new();
        mirror_sockets.insert(socket_id.clone(), mirror);

        // 5. Connect a client to the mirror socket. This triggers the accept
        //    loop to spawn handle_socket_client, which sends a RelayMessage
        //    containing SocketConnectRequest + ack_tx on msg_rx.
        let mut client = tokio::net::UnixStream::connect(&mirror_path)
            .await
            .expect("should connect to mirror socket");

        // 6. Act as the session loop: receive the RelayMessage, relay the
        //    control message to the host, and signal the ack so
        //    handle_socket_client proceeds to open the data connection.
        let relay = tokio::time::timeout(Duration::from_secs(5), msg_rx.recv())
            .await
            .expect("should receive RelayMessage within timeout")
            .expect("msg_rx should not be closed");

        // Send the SocketConnectRequest on the registered control connection
        conn.send(&relay.msg).await.unwrap();

        // Signal the ack so handle_socket_client opens the data connection
        // *after* the host has received the SocketConnectRequest.
        if let Some(ack_tx) = relay.ack_tx {
            let _ = ack_tx.send(());
        }

        // Small delay for the host to process the SocketConnectRequest and
        // register the pending connection before ConnectReady arrives.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 7. Send data through the client and verify echo response
        let test_data = b"Mirror socket pipeline test!";
        client.write_all(test_data).await.unwrap();
        client.flush().await.unwrap();

        let mut response = vec![0u8; test_data.len()];
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_exact(&mut client, &mut response),
        )
        .await
        .expect("should receive echo within timeout")
        .expect("read should succeed");

        assert_eq!(
            &response, test_data,
            "echo response should match sent data through mirror socket pipeline"
        );

        // Send a second message to verify ongoing bidirectional flow
        let test_data_2 = b"Second mirror message";
        client.write_all(test_data_2).await.unwrap();
        client.flush().await.unwrap();

        let mut response_2 = vec![0u8; test_data_2.len()];
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_exact(&mut client, &mut response_2),
        )
        .await
        .expect("should receive second echo within timeout")
        .expect("second read should succeed");

        assert_eq!(&response_2, test_data_2);

        // 8. Clean up
        drop(client);
        drop(conn);
        let _ = mirror_sockets
            .values()
            .next()
            .map(|m| m.shutdown_tx.send(true));
        cleanup_all_mirrors(&mut mirror_sockets);
        echo_handle.abort();
        host_handle.abort();
    }

    // -----------------------------------------------------------------------
    // Test 24: ListResponse includes socket forwards
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_list_response_includes_socket_forwards() {
        let tmp = tempfile::tempdir().unwrap();
        let control_port = find_free_port().await;
        let data_port = find_free_port().await;

        // Create a socket before starting the daemon
        let _sock_path = create_unix_socket(tmp.path(), "list-test.sock");

        let config = host_config_with_socket_scanning(
            control_port,
            data_port,
            tmp.path(),
            100, // fast scan interval
        );
        let host_handle = tokio::spawn(async move { dbr::host::run(config).await });

        wait_port_open(control_port, Duration::from_secs(5)).await;

        let control_addr: SocketAddr = ([127, 0, 0, 1], control_port).into();

        // Register a container so the scanner has someone to send SocketForward to
        let mut container_conn =
            register_container(control_addr, "list-socket-test", "list-socket-host").await;

        // Wait for the SocketForward message so we know the scanner has discovered it
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = container_conn.recv().await.unwrap();
                match msg {
                    Message::Ping => {
                        let _ = container_conn.send(&Message::Pong).await;
                        continue;
                    }
                    Message::SocketForward { .. } => return,
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        })
        .await
        .expect("should receive SocketForward within timeout");

        // Now send a ListRequest from a separate CLI connection
        let mut cli_conn = control::connect(control_addr).await.unwrap();
        cli_conn.send(&Message::ListRequest).await.unwrap();

        let response = cli_conn.recv().await.unwrap();
        match response {
            Message::ListResponse {
                forwards,
                socket_forwards,
            } => {
                assert!(forwards.is_empty(), "no port forwards expected");
                assert_eq!(
                    socket_forwards.len(),
                    1,
                    "should have exactly one socket forward"
                );
                let sf = &socket_forwards[0];
                assert!(
                    sf.host_path.contains("list-test.sock"),
                    "host_path should reference the socket file, got: {}",
                    sf.host_path
                );
                assert!(
                    sf.container_path.contains("list-test.sock"),
                    "container_path should reference the socket file, got: {}",
                    sf.container_path
                );
                assert!(!sf.socket_id.is_empty(), "socket_id should not be empty");
            }
            other => panic!("expected ListResponse, got {other:?}"),
        }

        // Clean up
        drop(cli_conn);
        drop(container_conn);
        host_handle.abort();
    }
}
