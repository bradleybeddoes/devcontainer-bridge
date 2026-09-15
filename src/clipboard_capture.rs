//! Native host clipboard image capture with bounded subprocess output.

use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::protocol::ClipboardFormat;

/// Largest clipboard image accepted by the bridge (20 MiB).
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 4096;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Failures while reading a native clipboard image.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// No supported image representation or usable clipboard tool was found.
    #[error("no requested PNG/JPEG image available; on Linux install wl-clipboard (Wayland) or xclip (X11) and run the host daemon in your desktop session")]
    Unavailable,
    /// A clipboard helper exceeded the time limit.
    #[error("clipboard capture timed out")]
    Timeout,
    /// A helper produced more output than permitted.
    #[error("clipboard helper output exceeds the permitted size")]
    TooLarge,
    /// A helper or its output pipe failed.
    #[error("clipboard helper I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The requested platform has no implemented clipboard backend.
    #[error("clipboard image capture is supported only on macOS and Linux")]
    UnsupportedPlatform,
}

/// Read a PNG or JPEG representation from the host's desktop clipboard.
///
/// On macOS, a single local file URL takes precedence over rendered clipboard
/// representations so copying a PNG/JPEG in Finder transfers the file rather
/// than its icon. Otherwise automatic selection prefers PNG. Explicit formats
/// require that representation to be supplied by the clipboard. PNG also
/// supports converting a TIFF-only clipboard image. No clipboard content, file
/// paths, or helper diagnostics are logged.
///
/// # Errors
///
/// Returns an error when no requested image is available, the desktop clipboard
/// cannot be reached, or the helper exceeds its time or output limit.
pub async fn capture(format: ClipboardFormat) -> Result<Vec<u8>, CaptureError> {
    #[cfg(target_os = "macos")]
    return capture_format(format).await;

    #[cfg(not(target_os = "macos"))]
    capture_candidates(format).await
}

#[cfg(not(target_os = "macos"))]
async fn capture_candidates(format: ClipboardFormat) -> Result<Vec<u8>, CaptureError> {
    let formats: &[ClipboardFormat] = match format {
        ClipboardFormat::Auto => &[ClipboardFormat::Png, ClipboardFormat::Jpeg],
        ClipboardFormat::Png => &[ClipboardFormat::Png],
        ClipboardFormat::Jpeg => &[ClipboardFormat::Jpeg],
    };
    for &candidate in formats {
        match capture_format(candidate).await {
            Ok(bytes) if matches_format(&bytes, candidate) => return Ok(bytes),
            Ok(_) | Err(CaptureError::Unavailable) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CaptureError::Unavailable)
}

fn matches_format(bytes: &[u8], format: ClipboardFormat) -> bool {
    match format {
        ClipboardFormat::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        ClipboardFormat::Jpeg => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        ClipboardFormat::Auto => false,
    }
}

#[cfg(target_os = "macos")]
async fn capture_format(format: ClipboardFormat) -> Result<Vec<u8>, CaptureError> {
    capture_format_from_pasteboard(format, None).await
}

#[cfg(target_os = "macos")]
async fn capture_format_from_pasteboard(
    format: ClipboardFormat,
    pasteboard_name: Option<&str>,
) -> Result<Vec<u8>, CaptureError> {
    let native_types = match format {
        ClipboardFormat::Png => "['public.png']",
        ClipboardFormat::Jpeg => "['public.jpeg']",
        ClipboardFormat::Auto => "['public.png', 'public.jpeg']",
    };
    let allow_tiff = !matches!(format, ClipboardFormat::Jpeg);
    // Only fixed enum-derived values enter this script. Writing NSData directly
    // avoids text encodings, legacy AppleScript clipboard classes and temp files.
    let script = format!(
        r#"ObjC.import('AppKit');
function run(argv) {{
    const board = argv.length === 0
        ? $.NSPasteboard.generalPasteboard
        : $.NSPasteboard.pasteboardWithName(argv[0]);
    const changeCount = board.changeCount;
    const fileUrls = $.NSMutableArray.array;
    const items = board.pasteboardItems;
    if (!items.isNil()) {{
        for (let i = 0; i < items.count; i++) {{
            const value = items.objectAtIndex(i).stringForType('public.file-url');
            if (!value.isNil()) {{
                const fileUrl = $.NSURL.URLWithString(value);
                if (fileUrl.isNil() || !fileUrl.isFileURL)
                    throw new Error('Expected a local file');
                fileUrls.addObject(fileUrl);
            }}
        }}
    }}
    let data = $();

    if (fileUrls.count > 0) {{
        if (fileUrls.count !== 1) throw new Error('Expected one image file');
        const fileUrl = fileUrls.objectAtIndex(0);
        const handle = $.NSFileHandle.fileHandleForReadingAtPath(fileUrl.path);
        if (handle.isNil()) throw new Error('Image file is unavailable');
        data = handle.readDataOfLength({MAX_IMAGE_BYTES_PLUS_ONE});
        if (board.changeCount !== changeCount) throw new Error('Clipboard changed');
    }} else {{
        const types = {native_types};
        for (let i = 0; i < types.length; i++) {{
            data = board.dataForType(types[i]);
            if (!data.isNil()) break;
        }}
        if (data.isNil() && {allow_tiff}) {{
            const tiff = board.dataForType('public.tiff');
            if (!tiff.isNil()) {{
                if (tiff.length > {MAX_IMAGE_BYTES}) throw new Error('Image too large');
                const bitmap = $.NSBitmapImageRep.imageRepWithData(tiff);
                if (!bitmap.isNil()) {{
                    if (bitmap.pixelsWide * bitmap.pixelsHigh > 40000000)
                        throw new Error('Image dimensions too large');
                    data = bitmap.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({{}}));
                }}
            }}
        }}
    }}
    if (data.isNil()) throw new Error('No requested image');
    if (data.length > {MAX_IMAGE_BYTES}) throw new Error('Image too large');
    $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
}}"#,
        MAX_IMAGE_BYTES_PLUS_ONE = MAX_IMAGE_BYTES + 1
    );
    let mut command = Command::new("/usr/bin/osascript");
    command.args(["-l", "JavaScript", "-e", &script]);
    if let Some(name) = pasteboard_name {
        command.arg(name);
    }
    let bytes = run_helper(&mut command, MAX_IMAGE_BYTES, CAPTURE_TIMEOUT).await?;
    if matches_format(&bytes, format)
        || (matches!(format, ClipboardFormat::Auto)
            && (matches_format(&bytes, ClipboardFormat::Png)
                || matches_format(&bytes, ClipboardFormat::Jpeg)))
    {
        Ok(bytes)
    } else {
        Err(CaptureError::Unavailable)
    }
}

#[cfg(target_os = "linux")]
async fn capture_format(format: ClipboardFormat) -> Result<Vec<u8>, CaptureError> {
    let mime = match format {
        ClipboardFormat::Png => "image/png",
        ClipboardFormat::Jpeg => "image/jpeg",
        ClipboardFormat::Auto => return Err(CaptureError::Unavailable),
    };
    for (program, args) in [
        ("wl-paste", vec!["--no-newline", "--type", mime]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-out", "-target", mime],
        ),
    ] {
        match run_helper(
            Command::new(program).args(args),
            MAX_IMAGE_BYTES,
            CAPTURE_TIMEOUT,
        )
        .await
        {
            Ok(bytes) if matches_format(&bytes, format) => return Ok(bytes),
            Ok(_) | Err(CaptureError::Unavailable) => continue,
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(CaptureError::Unavailable)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn capture_format(_format: ClipboardFormat) -> Result<Vec<u8>, CaptureError> {
    Err(CaptureError::UnsupportedPlatform)
}

async fn read_bounded(
    reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, CaptureError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(CaptureError::TooLarge);
    }
    Ok(bytes)
}

async fn run_helper(
    command: &mut Command,
    output_limit: usize,
    deadline: Duration,
) -> Result<Vec<u8>, CaptureError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("clipboard helper stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("clipboard helper stderr pipe unavailable"))?;
    let result = tokio::time::timeout(deadline, async {
        let (bytes, _, status) = tokio::try_join!(
            read_bounded(stdout, output_limit),
            read_bounded(stderr, MAX_STDERR_BYTES),
            async { child.wait().await.map_err(CaptureError::Io) }
        )?;
        if !status.success() {
            return Err(CaptureError::Unavailable);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| CaptureError::Timeout)
    .and_then(|result| result);
    if result.is_err() {
        // Reap the direct helper on error, including an output-limit failure.
        // kill_on_drop also covers cancellation of the enclosing capture future.
        let _ = child.kill().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use std::path::Path;

    #[test]
    fn signatures_must_match_requested_format() {
        assert!(matches_format(
            b"\x89PNG\r\n\x1a\nrest",
            ClipboardFormat::Png
        ));
        assert!(matches_format(
            &[0xff, 0xd8, 0xff, 0xe0],
            ClipboardFormat::Jpeg
        ));
        assert!(!matches_format(b"\x89PNG\r\n\x1a\n", ClipboardFormat::Jpeg));
        assert!(!matches_format(b"plain text", ClipboardFormat::Png));
        assert!(!matches_format(b"", ClipboardFormat::Auto));
    }

    #[cfg(target_os = "macos")]
    async fn seed_file_pasteboard(
        name: &str,
        sources: &[&Path],
        rendered_image: &Path,
    ) -> Result<(), CaptureError> {
        let script = r#"ObjC.import('AppKit');
function run(argv) {
    const board = $.NSPasteboard.pasteboardWithName(argv[0]);
    const rendered = $.NSData.dataWithContentsOfFile(argv[argv.length - 1]);
    if (rendered.isNil()) throw new Error('Missing rendered image fixture');
    const items = $.NSMutableArray.array;
    for (let i = 1; i < argv.length - 1; i++) {
        const item = $.NSPasteboardItem.alloc.init;
        const fileUrl = $.NSURL.fileURLWithPath(argv[i]);
        item.setStringForType(fileUrl.absoluteString, 'public.file-url');
        item.setDataForType(rendered, 'public.png');
        items.addObject(item);
    }
    board.clearContents;
    if (!board.writeObjects(items)) throw new Error('Unable to seed pasteboard');
}"#;
        let mut command = Command::new("/usr/bin/osascript");
        command.args(["-l", "JavaScript", "-e", script, name]);
        for source in sources {
            command.arg(source);
        }
        command.arg(rendered_image);
        run_helper(&mut command, 0, CAPTURE_TIMEOUT)
            .await
            .map(|_| ())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn finder_file_bytes_take_precedence_over_rendered_icon() {
        let root = tempfile::tempdir().unwrap();
        let png = root.path().join("actual image.png");
        let jpeg = root.path().join("actual image.jpg");
        let unsupported = root.path().join("notes.txt");
        let icon = root.path().join("finder-icon.png");
        let png_bytes = b"\x89PNG\r\n\x1a\nactual-png";
        let jpeg_bytes = b"\xff\xd8\xffactual-jpeg";
        let icon_bytes = b"\x89PNG\r\n\x1a\nfinder-icon";
        std::fs::write(&png, png_bytes).unwrap();
        std::fs::write(&jpeg, jpeg_bytes).unwrap();
        std::fs::write(&unsupported, b"not an image").unwrap();
        std::fs::write(&icon, icon_bytes).unwrap();

        for (source, format, expected) in [
            (&png, ClipboardFormat::Auto, png_bytes.as_slice()),
            (&png, ClipboardFormat::Png, png_bytes.as_slice()),
            (&jpeg, ClipboardFormat::Auto, jpeg_bytes.as_slice()),
            (&jpeg, ClipboardFormat::Jpeg, jpeg_bytes.as_slice()),
        ] {
            let name = format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4());
            seed_file_pasteboard(&name, &[source.as_path()], &icon)
                .await
                .unwrap();
            assert_eq!(
                capture_format_from_pasteboard(format, Some(&name))
                    .await
                    .unwrap(),
                expected
            );
        }

        let name = format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4());
        seed_file_pasteboard(&name, &[unsupported.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_format_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));

        let name = format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4());
        seed_file_pasteboard(&name, &[jpeg.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_format_from_pasteboard(ClipboardFormat::Png, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));

        let name = format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4());
        seed_file_pasteboard(&name, &[png.as_path(), jpeg.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_format_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));

        let oversized = root.path().join("oversized.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        let name = format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4());
        seed_file_pasteboard(&name, &[oversized.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_format_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn bounded_reader_accepts_exact_limit() {
        assert_eq!(read_bounded(&b"1234"[..], 4).await.unwrap(), b"1234");
        assert!(matches!(
            read_bounded(&b"12345"[..], 4).await,
            Err(CaptureError::TooLarge)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_preserves_binary_output() {
        let bytes = run_helper(
            Command::new("sh").args(["-c", "printf '\\000\\377\\012'"]),
            3,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(bytes, [0, 255, 10]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_rejects_excess_output() {
        let result = run_helper(
            Command::new("sh").args(["-c", "printf 12345"]),
            4,
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Err(CaptureError::TooLarge)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_rejects_excess_stderr() {
        let result = run_helper(
            Command::new("sh").args(["-c", "while :; do printf diagnostic >&2; done"]),
            4,
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Err(CaptureError::TooLarge)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_times_out() {
        let result = run_helper(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            4,
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Err(CaptureError::Timeout)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_failure_does_not_expose_diagnostics() {
        let error = run_helper(
            Command::new("sh").args(["-c", "printf secret >&2; exit 1"]),
            4,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CaptureError::Unavailable));
        assert!(!error.to_string().contains("secret"));
    }
}
