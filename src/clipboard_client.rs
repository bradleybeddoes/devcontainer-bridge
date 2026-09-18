//! Receive host clipboard images as private local files, optionally pasting their paths into tmux.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::clipboard_capture::{MAX_BATCH_BYTES, MAX_BATCH_IMAGES, MAX_IMAGE_BYTES};
use crate::control::{self, ControlError};
use crate::protocol::{ClipboardFormat, ClipboardToken, Message};

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const TMUX_TIMEOUT: Duration = Duration::from_secs(5);

/// Images saved by one clipboard transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedImages {
    /// Private directory holding every image of this transfer.
    pub directory: PathBuf,
    /// Saved image files, in clipboard order.
    pub paths: Vec<PathBuf>,
}

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
    /// Images are saved, but tmux insertion failed.
    #[error("images saved in {directory}, but tmux insertion failed: {reason}")]
    TmuxPaste {
        /// The retained transfer directory.
        directory: PathBuf,
        /// The tmux error.
        reason: String,
    },
}

/// Save every PNG or JPEG from the host clipboard and optionally type their quoted paths into tmux.
///
/// `addr` is the host data port. Copying several image files on the host saves
/// one file per image in a single transfer directory. A tmux target must be an
/// exact pane ID (`%123`) on the server selected by the inherited `TMUX`
/// environment variable. Paths are inserted literally, shell-quoted and each
/// followed by a space, without an Enter key. Successful files are retained
/// until the caller removes them.
///
/// # Errors
/// Returns an error for a failed transfer, invalid image signature, unsafe output
/// path, or invalid tmux pane. A tmux insertion failure retains the completed
/// images and reports their directory in [`PasteError::TmuxPaste`].
pub async fn paste(
    addr: SocketAddr,
    auth_token: &str,
    format: ClipboardFormat,
    output_dir: &Path,
    tmux_target: Option<&str>,
) -> Result<SavedImages, PasteError> {
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
        receive_images(&mut BufReader::new(stream), format).await
    };
    let images = tokio::time::timeout(TRANSFER_TIMEOUT, transfer)
        .await
        .map_err(|_| PasteError::Timeout)??;
    let saved = save_images(&output_dir, &images)?;
    if let Some(target) = tmux_target {
        let mut quoted = String::new();
        for path in &saved.paths {
            quoted.push_str(&quote_path(path)?);
            quoted.push(' ');
        }
        tmux(&["send-keys", "-l", "-t", target, "--", &quoted])
            .await
            .map_err(|reason| PasteError::TmuxPaste {
                directory: saved.directory.clone(),
                reason,
            })?;
    }
    Ok(saved)
}

async fn receive_images<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    requested: ClipboardFormat,
) -> Result<Vec<(ClipboardFormat, Vec<u8>)>, PasteError> {
    // A single image arrives without a count so older hosts remain readable.
    let mut buffered = None;
    let count = match control::read_message(reader).await? {
        Message::ClipboardBatchReady { count } => count,
        message => {
            buffered = Some(message);
            1
        }
    };
    if count == 0 || count as usize > MAX_BATCH_IMAGES {
        return Err(PasteError::InvalidResponse(
            "image count is outside the allowed range",
        ));
    }
    let mut images = Vec::with_capacity(count as usize);
    let mut total: u64 = 0;
    for _ in 0..count {
        let message = match buffered.take() {
            Some(message) => message,
            None => control::read_message(reader).await?,
        };
        let (format, size) = match message {
            Message::ClipboardReady { format, size } => (format, size),
            Message::ClipboardError { error } => return Err(PasteError::Host(error)),
            _ => return Err(PasteError::InvalidResponse("expected ClipboardReady")),
        };
        if format == ClipboardFormat::Auto
            || (requested != ClipboardFormat::Auto && requested != format)
        {
            return Err(PasteError::InvalidResponse("unexpected image format"));
        }
        total = total.saturating_add(size);
        if size == 0 || size > MAX_IMAGE_BYTES as u64 || total > MAX_BATCH_BYTES as u64 {
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
        images.push((format, bytes));
    }
    Ok(images)
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

fn save_images(
    dir: &Path,
    images: &[(ClipboardFormat, Vec<u8>)],
) -> Result<SavedImages, PasteError> {
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
    let mut paths = Vec::with_capacity(images.len());
    for (index, (format, bytes)) in images.iter().enumerate() {
        let extension = match format {
            ClipboardFormat::Png => "png",
            ClipboardFormat::Jpeg => "jpg",
            ClipboardFormat::Auto => {
                return Err(PasteError::InvalidResponse("unspecified image format"))
            }
        };
        let filename = if images.len() == 1 {
            format!("clipboard.{extension}")
        } else {
            format!("clipboard-{}.{extension}", index + 1)
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
        paths.push(path);
    }
    // TempDir removes partial files on every earlier error.
    let directory = transfer_dir.keep();
    Ok(SavedImages { directory, paths })
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

    fn batched(count: u32, frames: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = serde_json::to_vec(&Message::ClipboardBatchReady { count }).unwrap();
        stream.push(b'\n');
        for frame in frames {
            stream.extend_from_slice(frame);
        }
        stream
    }

    #[tokio::test]
    async fn keeps_image_bytes_buffered_with_header() {
        for (format, bytes) in [
            (ClipboardFormat::Png, b"\x89PNG\r\n\x1a\nhello".as_slice()),
            (ClipboardFormat::Jpeg, b"\xff\xd8\xffhello".as_slice()),
        ] {
            let frame = framed(format, bytes.len() as u64, bytes);
            let mut reader = BufReader::new(frame.as_slice());
            let images = receive_images(&mut reader, ClipboardFormat::Auto)
                .await
                .unwrap();
            assert_eq!(images, vec![(format, bytes.to_vec())]);
        }
    }

    #[tokio::test]
    async fn reads_every_image_announced_by_a_batch_header() {
        let png = b"\x89PNG\r\n\x1a\nfirst".as_slice();
        let jpeg = b"\xff\xd8\xffsecond".as_slice();
        let frames = [
            framed(ClipboardFormat::Png, png.len() as u64, png),
            framed(ClipboardFormat::Jpeg, jpeg.len() as u64, jpeg),
        ];
        let stream = batched(2, &frames);
        let images = receive_images(
            &mut BufReader::new(stream.as_slice()),
            ClipboardFormat::Auto,
        )
        .await
        .unwrap();
        assert_eq!(
            images,
            vec![
                (ClipboardFormat::Png, png.to_vec()),
                (ClipboardFormat::Jpeg, jpeg.to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn rejects_truncated_oversized_and_mismatched_images() {
        let png = b"\x89PNG\r\n\x1a\n".as_slice();
        let frame = || framed(ClipboardFormat::Png, png.len() as u64, png);
        for stream in [
            framed(ClipboardFormat::Png, 100, b"\x89PNG\r\n\x1a\n"),
            framed(ClipboardFormat::Png, MAX_IMAGE_BYTES as u64 + 1, b""),
            framed(ClipboardFormat::Png, 3, b"bad"),
            framed(ClipboardFormat::Auto, 3, b"bad"),
            framed(ClipboardFormat::Png, 0, b""),
            // A batch that stops short of its announced count.
            batched(2, &[frame()]),
            batched(0, &[]),
            batched(MAX_BATCH_IMAGES as u32 + 1, &[frame()]),
            batched(
                2,
                &[
                    framed(ClipboardFormat::Png, MAX_BATCH_BYTES as u64, png),
                    framed(ClipboardFormat::Png, MAX_BATCH_BYTES as u64, png),
                ],
            ),
        ] {
            assert!(receive_images(
                &mut BufReader::new(stream.as_slice()),
                ClipboardFormat::Auto
            )
            .await
            .is_err());
        }
        let stream = framed(ClipboardFormat::Jpeg, 3, b"\xff\xd8\xff");
        assert!(
            receive_images(&mut BufReader::new(stream.as_slice()), ClipboardFormat::Png)
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
        let first =
            save_images(&physical_root, &[(ClipboardFormat::Png, b"first".to_vec())]).unwrap();
        let second = save_images(
            &physical_root,
            &[(ClipboardFormat::Jpeg, b"second".to_vec())],
        )
        .unwrap();
        assert_ne!(first.directory, second.directory);
        assert_eq!(
            first.paths,
            vec![first.directory.join("clipboard.png")],
            "a lone image keeps its unnumbered name"
        );
        assert_eq!(second.paths, vec![second.directory.join("clipboard.jpg")]);
        assert_eq!(std::fs::read(&first.paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&second.paths[0]).unwrap(), b"second");
        assert!(save_images(&physical_root, &[(ClipboardFormat::Auto, b"bad".to_vec())]).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first.paths[0])
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&first.directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn numbers_several_images_inside_one_transfer_directory() {
        let root = tempfile::tempdir().unwrap();
        let physical_root = root.path().canonicalize().unwrap();
        let saved = save_images(
            &physical_root,
            &[
                (ClipboardFormat::Png, b"first".to_vec()),
                (ClipboardFormat::Jpeg, b"second".to_vec()),
                (ClipboardFormat::Png, b"third".to_vec()),
            ],
        )
        .unwrap();
        assert_eq!(
            saved.paths,
            ["clipboard-1.png", "clipboard-2.jpg", "clipboard-3.png"]
                .map(|name| saved.directory.join(name))
                .to_vec()
        );
        assert_eq!(std::fs::read(&saved.paths[2]).unwrap(), b"third");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);

        // A later unwritable image discards the whole transfer.
        assert!(save_images(
            &physical_root,
            &[
                (ClipboardFormat::Png, b"first".to_vec()),
                (ClipboardFormat::Auto, b"bad".to_vec()),
            ],
        )
        .is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
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
        assert!(save_images(&link, &[(ClipboardFormat::Png, b"data".to_vec())]).is_err());
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
