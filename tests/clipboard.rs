//! Real CLI and wire transfer tests; desktop helpers are isolated subprocess fixtures.
#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use dbr::control;
use dbr::protocol::Message;
use tokio::io::BufReader;
use tokio::net::TcpStream;

const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Daemon {
    child: Child,
    root: tempfile::TempDir,
    data_port: u16,
    control_port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    async fn start(enabled: bool, no_auth: bool, png: Option<&[u8]>, jpeg: Option<&[u8]>) -> Self {
        Self::start_with_config(enabled, no_auth, png, jpeg, None, false).await
    }

    async fn start_with_config(
        enabled: bool,
        no_auth: bool,
        png: Option<&[u8]>,
        jpeg: Option<&[u8]>,
        configured: Option<bool>,
        disabled: bool,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        if let Some(enabled) = configured {
            write_clipboard_config(root.path(), enabled);
        }
        for (extension, bytes) in [("png", png), ("jpeg", jpeg)] {
            if let Some(bytes) = bytes {
                fs::write(root.path().join(extension), bytes).unwrap();
            }
        }
        // PATH applies only to the daemon. Neither real desktop clipboard can be reached.
        for name in ["wl-paste", "xclip"] {
            let helper = root.path().join(name);
            fs::write(
                &helper,
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$DBR_FIXTURES/calls"
for arg do
    case "$arg" in
        image/png) exec /bin/cat "$DBR_FIXTURES/png" ;;
        image/jpeg) exec /bin/cat "$DBR_FIXTURES/jpeg" ;;
    esac
done
exit 1
"#,
            )
            .unwrap();
            fs::set_permissions(helper, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let control_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let control_port = control_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbr"));
        command
            .args([
                "host-daemon",
                "--bind-addr",
                "127.0.0.1",
                "--control-port",
                &control_port.to_string(),
                "--data-port",
                &data_port.to_string(),
                "--auth-token",
                TOKEN,
                "--no-socket-forwarding",
            ])
            .env_remove("DCBRIDGE_AUTH_TOKEN")
            .env_remove("DCBRIDGE_AUTH_TOKEN_FILE")
            .env("PATH", root.path())
            .env("HOME", root.path())
            .env("DBR_FIXTURES", root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(root.path().join("daemon.log")).unwrap(),
            ));
        if enabled {
            command.arg("--allow-clipboard");
        }
        if no_auth {
            command.arg("--no-auth");
        }
        if disabled {
            command.arg("--no-clipboard");
        }
        drop(control_listener);
        drop(data_listener);
        let child = command.spawn().unwrap();
        let mut daemon = Self {
            child,
            root,
            data_port,
            control_port,
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    daemon.child.try_wait().unwrap().is_none(),
                    "daemon exited: {}",
                    fs::read_to_string(daemon.root.path().join("daemon.log")).unwrap()
                );
                let probe = async {
                    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", control_port)).await
                    else {
                        return false;
                    };
                    if control::write_message(&mut stream, &Message::Ping)
                        .await
                        .is_err()
                    {
                        return false;
                    }
                    matches!(
                        control::read_message(&mut BufReader::new(stream)).await,
                        Ok(Message::Pong)
                    ) && TcpStream::connect(("127.0.0.1", data_port)).await.is_ok()
                };
                // Retry transient connection failures within the startup deadline,
                // spacing probes so they do not flood the daemon's accept loop.
                if tokio::time::timeout(Duration::from_millis(500), probe)
                    .await
                    .unwrap_or(false)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("host daemon did not become ready");
        daemon
    }

    fn output_dir(&self) -> PathBuf {
        self.root.path().join("received images 'quoted' $(literal)")
    }

    fn paste_command(&self, token: &str, format: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dbr"));
        command
            .args([
                "paste",
                "--host",
                "127.0.0.1",
                "--data-port",
                &self.data_port.to_string(),
                "--auth-token",
                token,
                "--format",
                format,
                "--output-dir",
            ])
            .arg(self.output_dir())
            .env("HOME", self.root.path())
            .kill_on_drop(true);
        command
    }

    async fn paste(&self, token: &str, format: &str) -> Output {
        run(&mut self.paste_command(token, format)).await
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.root.path().join("calls")).unwrap_or_default()
    }
}

async fn run(command: &mut tokio::process::Command) -> Output {
    command
        .env_remove("DCBRIDGE_AUTH_TOKEN")
        .env_remove("DCBRIDGE_AUTH_TOKEN_FILE");
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("CLI timed out")
        .unwrap()
}

fn saved_path(output: Output) -> PathBuf {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert!(path.is_absolute());
    path
}

#[tokio::test]
async fn png_larger_than_control_frame_is_preserved_and_private() {
    // Signature plus opaque binary payload tests transport, not image decoding.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend((0..100_000).map(|i| (i % 256) as u8));
    let daemon = Daemon::start(true, false, Some(&png), Some(b"\xff\xd8\xffjpeg")).await;
    let first = saved_path(daemon.paste(TOKEN, "auto").await);
    let second = saved_path(daemon.paste(TOKEN, "png").await);
    assert_ne!(first, second);
    for path in [first, second] {
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(fs::read(&path).unwrap(), png);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(!daemon.calls().contains("image/jpeg"));
}

#[tokio::test]
async fn auto_falls_back_to_jpeg_and_explicit_jpeg_skips_png() {
    let jpeg = b"\xff\xd8\xffjpeg\0binary\xff\xd9";
    let daemon = Daemon::start(true, false, None, Some(jpeg)).await;
    let path = saved_path(daemon.paste(TOKEN, "auto").await);
    assert_eq!(fs::read(path).unwrap(), jpeg);
    assert!(daemon.calls().contains("image/png"));
    assert!(daemon.calls().contains("image/jpeg"));
    fs::remove_file(daemon.root.path().join("calls")).unwrap();
    let path = saved_path(daemon.paste(TOKEN, "jpeg").await);
    assert_eq!(path.extension().unwrap(), "jpg");
    assert_eq!(fs::read(path).unwrap(), jpeg);
    assert!(!daemon.calls().contains("image/png"));
}

#[tokio::test]
async fn clipboard_requires_opt_in_and_authentication_before_capture() {
    for (enabled, no_auth, token) in [
        (
            true,
            false,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
        (false, false, TOKEN),
        (false, true, TOKEN),
    ] {
        let daemon = Daemon::start(enabled, no_auth, Some(b"\x89PNG\r\n\x1a\nimage"), None).await;
        let output = daemon.paste(token, "png").await;
        assert!(!output.status.success());
        assert!(daemon.calls().is_empty(), "unauthorized helper invocation");
        assert!(!daemon.output_dir().exists());
    }
}

#[tokio::test]
async fn clipboard_opt_in_conflicts_with_no_auth() {
    let output = run(tokio::process::Command::new(env!("CARGO_BIN_EXE_dbr"))
        .args(["host-daemon", "--allow-clipboard", "--no-auth"])
        .kill_on_drop(true))
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

#[tokio::test]
async fn empty_clipboard_creates_no_artifact() {
    let daemon = Daemon::start(true, false, None, None).await;
    let output = daemon.paste(TOKEN, "auto").await;
    assert!(!output.status.success());
    assert!(daemon.calls().contains("image/png"));
    assert!(daemon.calls().contains("image/jpeg"));
    assert!(!daemon.output_dir().exists());
}

#[tokio::test]
async fn explicit_missing_token_file_does_not_fall_back_to_home_token() {
    let daemon = Daemon::start(true, false, Some(b"\x89PNG\r\n\x1a\nimage"), None).await;
    let config_dir = daemon.root.path().join(".config/dbr");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("auth-token"), TOKEN).unwrap();
    let output = run(tokio::process::Command::new(env!("CARGO_BIN_EXE_dbr"))
        .args([
            "paste",
            "--host",
            "127.0.0.1",
            "--data-port",
            &daemon.data_port.to_string(),
            "--auth-token-file",
        ])
        .arg(daemon.root.path().join("missing-token"))
        .arg("--output-dir")
        .arg(daemon.output_dir())
        .env("HOME", daemon.root.path())
        .env_remove("DCBRIDGE_AUTH_TOKEN")
        .kill_on_drop(true))
    .await;
    assert!(!output.status.success());
    assert!(daemon.calls().is_empty());
    assert!(!daemon.output_dir().exists());
}

fn write_clipboard_config(home: &Path, enabled: bool) {
    let directory = home.join(".config/dbr");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("config.toml"),
        format!("[clipboard]\nenabled = {enabled}\n"),
    )
    .unwrap();
}

// ensure/restart intentionally detach the daemon, so cleanup uses the authenticated
// shutdown command against this test's own port, even when an assertion unwinds.
struct DetachedDaemon<'a>(&'a Daemon);

impl Drop for DetachedDaemon<'_> {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_dbr"))
            .args([
                "stop",
                "--host",
                "127.0.0.1",
                "--control-port",
                &self.0.control_port.to_string(),
                "--auth-token",
                TOKEN,
            ])
            .env("HOME", self.0.root.path())
            .env_remove("DCBRIDGE_AUTH_TOKEN")
            .env_remove("DCBRIDGE_AUTH_TOKEN_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
async fn clipboard_config_survives_ensure_and_restart() {
    let png = b"\x89PNG\r\n\x1a\nconfigured image";
    let mut daemon = Daemon::start(false, false, Some(png), None).await;
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    write_clipboard_config(daemon.root.path(), true);
    let cleanup = DetachedDaemon(&daemon);
    let pid_file = daemon.root.path().join(".config/dbr/daemon.pid");
    let mut previous_pid = None;
    for action in ["ensure", "restart"] {
        let output = run(tokio::process::Command::new(env!("CARGO_BIN_EXE_dbr"))
            .args([
                action,
                "--host",
                "127.0.0.1",
                "--control-port",
                &daemon.control_port.to_string(),
                "--data-port",
                &daemon.data_port.to_string(),
                "--auth-token",
                TOKEN,
            ])
            .env("HOME", daemon.root.path())
            .env("PATH", daemon.root.path())
            .env("DBR_FIXTURES", daemon.root.path())
            .kill_on_drop(true))
        .await;
        assert!(
            output.status.success(),
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert_ne!(
            previous_pid.as_ref(),
            Some(&pid),
            "restart retained the original daemon"
        );
        previous_pid = Some(pid);
        let image = saved_path(daemon.paste(TOKEN, "png").await);
        assert_eq!(fs::read(image).unwrap(), png);
    }
    drop(cleanup);
    assert!(TcpStream::connect(("127.0.0.1", daemon.control_port))
        .await
        .is_err());
}

#[tokio::test]
async fn clipboard_cli_policy_overrides_configuration() {
    let png = b"\x89PNG\r\n\x1a\nconfigured image";
    let enabled = Daemon::start_with_config(true, false, Some(png), None, Some(false), false).await;
    assert_eq!(
        fs::read(saved_path(enabled.paste(TOKEN, "png").await)).unwrap(),
        png
    );
    let disabled = Daemon::start_with_config(false, false, Some(png), None, Some(true), true).await;
    assert!(!disabled.paste(TOKEN, "png").await.status.success());
    assert!(disabled.calls().is_empty());
    assert!(!disabled.output_dir().exists());
}

#[tokio::test]
async fn clipboard_enabled_in_config_rejects_no_auth() {
    let daemon = Daemon::start(false, false, Some(b"\x89PNG\r\n\x1a\nimage"), None).await;
    write_clipboard_config(daemon.root.path(), true);
    let output = run(tokio::process::Command::new(env!("CARGO_BIN_EXE_dbr"))
        .args([
            "host-daemon",
            "--no-auth",
            "--bind-addr",
            "127.0.0.1",
            "--control-port",
            "0",
            "--data-port",
            "0",
        ])
        .env("HOME", daemon.root.path())
        .env("PATH", daemon.root.path())
        .env("DBR_FIXTURES", daemon.root.path())
        .kill_on_drop(true))
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("clipboard sharing requires authentication"));
    assert!(daemon.calls().is_empty());
}

struct TmuxServer(PathBuf);

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.0)
            .arg("kill-server")
            .output();
    }
}

impl TmuxServer {
    async fn command(&self, args: &[&str]) -> Output {
        run(tokio::process::Command::new("tmux")
            .arg("-S")
            .arg(&self.0)
            .args(args)
            .kill_on_drop(true))
        .await
    }
}

#[tokio::test]
async fn tmux_inserts_literal_quoted_path_without_submitting() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping tmux integration: tmux is not installed");
        return;
    }
    let daemon = Daemon::start(true, false, Some(b"\x89PNG\r\n\x1a\nimage"), None).await;
    let server = TmuxServer(daemon.root.path().join("tmux.sock"));
    let submitted = daemon.root.path().join("submitted");
    let output = run(tokio::process::Command::new("tmux")
        .arg("-S").arg(&server.0).args(["-f", "/dev/null", "new-session", "-d", "-P", "-F", "#{pane_id}", "-x", "500", "-y", "20", "/bin/sh", "-c", "printf READY; IFS= read -r line; printf '%s' \"$line\" > \"$DBR_SUBMITTED\"; exec /bin/cat"])
        .env("DBR_SUBMITTED", &submitted).env_remove("TMUX").kill_on_drop(true)).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pane = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let tmux_env = format!("{},0,0", server.0.display());
    let path = saved_path(
        run(daemon
            .paste_command(TOKEN, "auto")
            .args(["--tmux-target", &pane])
            .env("TMUX", tmux_env))
        .await,
    );
    let quoted = format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let output = server.command(&["capture-pane", "-p", "-t", &pane]).await;
            assert!(output.status.success());
            if String::from_utf8_lossy(&output.stdout).contains(&quoted) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("path was not echoed in target pane");
    assert!(!submitted.exists(), "paste submitted the input");
    assert!(server
        .command(&["send-keys", "-t", &pane, "Enter"])
        .await
        .status
        .success());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read_to_string(&submitted).ok().as_deref() == Some(format!("{quoted} ").as_str())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("explicit Enter did not submit the exact quoted path");
    // Exercise the documented run-shell command in the pane's server context.
    // An unattached test server cannot dispatch a physical prefix key.
    let config_dir = daemon.root.path().join(".config/dbr");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("data_port = {}\n", daemon.data_port),
    )
    .unwrap();
    for (key, value) in [
        ("HOME", daemon.root.path().to_str().unwrap().to_owned()),
        (
            "PATH",
            format!(
                "{}:/usr/bin:/bin",
                Path::new(env!("CARGO_BIN_EXE_dbr"))
                    .parent()
                    .unwrap()
                    .display()
            ),
        ),
        ("DCBRIDGE_HOST", "127.0.0.1".to_owned()),
        ("DCBRIDGE_AUTH_TOKEN", TOKEN.to_owned()),
    ] {
        assert!(server
            .command(&["set-environment", "-g", key, &value])
            .await
            .status
            .success());
    }
    let binding = "dbr paste --tmux-target \"#{pane_id}\" >/dev/null";
    assert!(server
        .command(&["bind-key", "V", "run-shell", "-b", binding])
        .await
        .status
        .success());
    let bindings = server.command(&["list-keys", "V"]).await;
    assert!(bindings.status.success());
    assert!(String::from_utf8_lossy(&bindings.stdout).contains("#{pane_id}"));
    assert!(server
        .command(&["run-shell", "-b", "-t", &pane, binding])
        .await
        .status
        .success());
    let default_dir = daemon.root.path().join(".cache/dbr/paste");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(entries) = fs::read_dir(&default_dir) {
                for entry in entries.flatten() {
                    let image = entry.path().join("clipboard.png");
                    if image.exists() {
                        let capture = server.command(&["capture-pane", "-p", "-t", &pane]).await;
                        if String::from_utf8_lossy(&capture.stdout)
                            .contains(image.to_str().unwrap())
                        {
                            assert_eq!(fs::read(&image).unwrap(), b"\x89PNG\r\n\x1a\nimage");
                            return;
                        }
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("documented run-shell command failed to insert image path");
    assert!(Path::new(&path).exists());
}
