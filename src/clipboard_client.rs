//! Receive host clipboard images as private local files, optionally pasting a path into tmux.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::clipboard_capture::MAX_IMAGE_BYTES;
use crate::control::{self, ControlError};
use crate::protocol::{ClipboardFormat, ClipboardToken, Message};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const TMUX_TIMEOUT: Duration = Duration::from_secs(5);

/// Failure to receive an image or insert its local path.
#[derive(Debug, Error)]
pub enum PasteError {
    /// Local filesystem or network failure.
    #[error("clipboard I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Invalid response framing.
    #[error("clipboard protocol error: {0}")]
    Protocol(#[from] ControlError),
    /// Host declined the clipboard request.
    #[error("host clipboard error: {0}")]
    Host(String),
    /// Invalid size, format, or contents.
    #[error("invalid clipboard response: {0}")]
    InvalidResponse(&'static str),
    /// Transfer did not complete before the deadline.
    #[error("clipboard transfer timed out after 30 seconds")]
    Timeout,
    /// Output path is unsafe to create or paste.
    #[error("invalid clipboard output path: {0}")]
    InvalidPath(&'static str),
    /// tmux target could not be validated before reading the clipboard.
    #[error("cannot use tmux target: {0}")]
    Tmux(String),
    /// Image is saved, but tmux insertion failed.
    #[error("image saved at {path}, but tmux insertion failed: {reason}")]
    TmuxPaste {
        /// The retained image file.
        path: PathBuf,
        /// The tmux error.
        reason: String,
    },
}

/// Save a PNG or JPEG from the host clipboard and optionally type its quoted path into tmux.
///
/// `addr` is the host data port. A tmux target must be an exact pane ID (`%123`)
/// on the server selected by the inherited `TMUX` environment variable. The
/// path is inserted literally, shell-quoted and followed by a space, without an Enter key. Successful
/// files are retained until the caller removes them.
///
/// # Errors
/// Returns an error for a failed transfer, invalid image signature, unsafe output
/// path, or invalid tmux pane. A tmux insertion failure retains the completed
/// image and reports its path in [`PasteError::TmuxPaste`].
pub async fn paste(
    addr: SocketAddr,
    auth_token: &str,
    format: ClipboardFormat,
    output_dir: &Path,
    tmux_target: Option<&str>,
) -> Result<PathBuf, PasteError> {
    if let Some(target) = tmux_target {
        validate_tmux(target).await?;
    }
    let output_dir = absolute_output_dir(output_dir)?;
    let transfer = async {
        let mut stream = TcpStream::connect(addr).await?;
        control::write_message(
            &mut stream,
            &Message::ClipboardRead {
                format,
                auth_token: ClipboardToken(auth_token.to_owned()),
            },
        )
        .await?;
        receive_image(&mut BufReader::new(stream), format).await
    };
    let (received_format, bytes) = tokio::time::timeout(TRANSFER_TIMEOUT, transfer)
        .await
        .map_err(|_| PasteError::Timeout)??;
    let path = save_image(&output_dir, received_format, &bytes)?;
    if let Some(target) = tmux_target {
        let quoted = format!("{} ", quote_path(&path)?);
        tmux(&["send-keys", "-l", "-t", target, "--", &quoted])
            .await
            .map_err(|reason| PasteError::TmuxPaste {
                path: path.clone(),
                reason,
            })?;
    }
    Ok(path)
}

async fn receive_image<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    requested: ClipboardFormat,
) -> Result<(ClipboardFormat, Vec<u8>), PasteError> {
    let (format, size) = match control::read_message(reader).await? {
        Message::ClipboardReady { format, size } => (format, size),
        Message::ClipboardError { error } => return Err(PasteError::Host(error)),
        _ => return Err(PasteError::InvalidResponse("expected ClipboardReady")),
    };
    if format == ClipboardFormat::Auto
        || (requested != ClipboardFormat::Auto && requested != format)
    {
        return Err(PasteError::InvalidResponse("unexpected image format"));
    }
    if size == 0 || size > MAX_IMAGE_BYTES as u64 {
        return Err(PasteError::InvalidResponse(
            "image size is outside the allowed range",
        ));
    }
    let mut bytes = vec![0; size as usize];
    // Retain the same reader: the JSON read may already have buffered image bytes.
    reader.read_exact(&mut bytes).await?;
    let matches = match format {
        ClipboardFormat::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        ClipboardFormat::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
        ClipboardFormat::Auto => false,
    };
    if !matches {
        return Err(PasteError::InvalidResponse(
            "image signature does not match its format",
        ));
    }
    Ok((format, bytes))
}

fn absolute_output_dir(path: &Path) -> Result<PathBuf, PasteError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    quote_path(&path)?;
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(PasteError::InvalidPath(
            "parent directory components are not supported",
        ));
    }
    Ok(path)
}

fn save_image(dir: &Path, format: ClipboardFormat, bytes: &[u8]) -> Result<PathBuf, PasteError> {
    // Refuse existing symlink components rather than writing through them.
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(PasteError::InvalidPath(
                    "output directory contains a symlink or non-directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("image-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let transfer_dir = builder.tempdir_in(dir)?;
    let filename = match format {
        ClipboardFormat::Png => "clipboard.png",
        ClipboardFormat::Jpeg => "clipboard.jpg",
        ClipboardFormat::Auto => {
            return Err(PasteError::InvalidResponse("unspecified image format"))
        }
    };
    let path = transfer_dir.path().join(filename);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // TempDir removes partial files on every earlier error.
    let _ = transfer_dir.keep();
    Ok(path)
}

fn quote_path(path: &Path) -> Result<String, PasteError> {
    let text = path
        .to_str()
        .ok_or(PasteError::InvalidPath("path is not UTF-8"))?;
    if text.chars().any(char::is_control) {
        return Err(PasteError::InvalidPath("path contains control characters"));
    }
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

async fn validate_tmux(target: &str) -> Result<(), PasteError> {
    if !target.starts_with('%')
        || target.len() < 2
        || !target[1..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err(PasteError::Tmux("use an exact pane ID such as %1".into()));
    }
    if std::env::var_os("TMUX").is_none_or(|value| value.is_empty()) {
        return Err(PasteError::Tmux(
            "TMUX must identify the destination server".into(),
        ));
    }
    let pane = tmux(&["display-message", "-p", "-t", target, "#{pane_id}"])
        .await
        .map_err(PasteError::Tmux)?;
    if pane.trim() != target {
        return Err(PasteError::Tmux("target pane did not match".into()));
    }
    Ok(())
}

async fn tmux(args: &[&str]) -> Result<String, String> {
    let output = tokio::time::timeout(
        TMUX_TIMEOUT,
        Command::new("tmux").args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "tmux timed out".to_owned())?
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(format: ClipboardFormat, size: u64, bytes: &[u8]) -> Vec<u8> {
        let mut frame = serde_json::to_vec(&Message::ClipboardReady { format, size }).unwrap();
        frame.push(b'\n');
        frame.extend_from_slice(bytes);
        frame
    }

    #[tokio::test]
    async fn keeps_image_bytes_buffered_with_header() {
        for (format, bytes) in [
            (ClipboardFormat::Png, b"\x89PNG\r\n\x1a\nhello".as_slice()),
            (ClipboardFormat::Jpeg, b"\xff\xd8\xffhello".as_slice()),
        ] {
            let frame = framed(format, bytes.len() as u64, bytes);
            let mut reader = BufReader::new(frame.as_slice());
            let (actual_format, actual_bytes) = receive_image(&mut reader, ClipboardFormat::Auto)
                .await
                .unwrap();
            assert_eq!(actual_format, format);
            assert_eq!(actual_bytes, bytes);
        }
    }

    #[tokio::test]
    async fn rejects_truncated_oversized_and_mismatched_images() {
        for frame in [
            framed(ClipboardFormat::Png, 100, b"\x89PNG\r\n\x1a\n"),
            framed(ClipboardFormat::Png, MAX_IMAGE_BYTES as u64 + 1, b""),
            framed(ClipboardFormat::Png, 3, b"bad"),
            framed(ClipboardFormat::Auto, 3, b"bad"),
            framed(ClipboardFormat::Png, 0, b""),
        ] {
            assert!(
                receive_image(&mut BufReader::new(frame.as_slice()), ClipboardFormat::Auto)
                    .await
                    .is_err()
            );
        }
        let frame = framed(ClipboardFormat::Jpeg, 3, b"\xff\xd8\xff");
        assert!(
            receive_image(&mut BufReader::new(frame.as_slice()), ClipboardFormat::Png)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_transfer_leaves_no_image_artifacts() {
        use tokio::io::AsyncWriteExt;
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            assert_eq!(
                control::read_message(&mut reader).await.unwrap(),
                Message::ClipboardRead {
                    format: ClipboardFormat::Png,
                    auth_token: ClipboardToken("test-token".into()),
                }
            );
            reader
                .get_mut()
                .write_all(&framed(ClipboardFormat::Png, 100, b"short"))
                .await
                .unwrap();
        });
        assert!(
            paste(addr, "test-token", ClipboardFormat::Png, root.path(), None)
                .await
                .is_err()
        );
        host.await.unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn private_unique_files_and_cleanup_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let physical_root = root.path().canonicalize().unwrap();
        let first = save_image(&physical_root, ClipboardFormat::Png, b"first").unwrap();
        let second = save_image(&physical_root, ClipboardFormat::Jpeg, b"second").unwrap();
        assert_ne!(first.parent(), second.parent());
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert!(save_image(&physical_root, ClipboardFormat::Auto, b"bad").is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(first.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_output_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let physical_root = root.path().canonicalize().unwrap();
        let destination = physical_root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let link = physical_root.join("link");
        std::os::unix::fs::symlink(&destination, &link).unwrap();
        assert!(save_image(&link, ClipboardFormat::Png, b"data").is_err());
        assert_eq!(std::fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn quotes_paths_and_rejects_controls() {
        assert_eq!(
            quote_path(Path::new("/tmp/a 'b' $(cmd).png")).unwrap(),
            "'/tmp/a '\\''b'\\'' $(cmd).png'"
        );
        assert!(quote_path(Path::new("/tmp/a\nb.png")).is_err());
        assert!(absolute_output_dir(Path::new("../images")).is_err());
    }
}
