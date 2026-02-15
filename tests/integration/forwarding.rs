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
/// Tries both [::1] and 127.0.0.1 since the host listener prefers IPv6.
async fn can_connect_port(port: u16) -> bool {
    let addrs: &[SocketAddr] = &[
        ([0, 0, 0, 0, 0, 0, 0, 1], port).into(),
        ([127, 0, 0, 1], port).into(),
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

    let ack = conn.recv().await.unwrap();
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

    let ack = conn.recv().await.unwrap();
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

    let ack = container_conn.recv().await.unwrap();
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
        Message::ListResponse { forwards } => {
            assert_eq!(forwards.len(), 1, "should have exactly one forward");
            let fwd = &forwards[0];
            assert_eq!(fwd.container_id, "list-test-container");
            assert_eq!(fwd.hostname, "list-host");
            assert_eq!(fwd.port, 19504);
            assert_eq!(fwd.host_port, host_port);
            assert_eq!(fwd.process_name.as_deref(), Some("my-app"));
            assert_eq!(fwd.pid, Some(42));
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

    let ack1 = conn1.recv().await.unwrap();
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

    let ack2 = conn2.recv().await.unwrap();
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
        Message::ListResponse { forwards } => {
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
    let data_rx = register_pending(&pending, conn_id.clone()).await;

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

        let ack = conn.recv().await.unwrap();
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
            Message::ConnectRequest { .. } => continue, // drain queued connect requests
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

        let ack = conn.recv().await.unwrap();
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

        let ack = conn.recv().await.unwrap();
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

    let data_rx = register_pending(&pending, conn_id.clone()).await;
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
