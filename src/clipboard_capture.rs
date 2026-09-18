//! Native host clipboard image capture with bounded subprocess output.

use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::protocol::ClipboardFormat;

/// Largest clipboard image accepted by the bridge (20 MiB).
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Largest number of images accepted from one clipboard selection.
pub const MAX_BATCH_IMAGES: usize = 16;
/// Largest combined size of one clipboard selection (64 MiB).
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 4096;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const MAX_HEADER_BYTES: usize = 256;

/// One image read from the host clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    /// Representation the clipboard supplied; never [`ClipboardFormat::Auto`].
    pub format: ClipboardFormat,
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

#[cfg(target_os = "macos")]
#[derive(serde::Deserialize)]
struct CaptureHeader {
    sizes: Vec<u64>,
}

/// Failures while reading a native clipboard image.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// No supported image representation or usable clipboard tool was found.
    #[error("no requested PNG/JPEG image available; on Linux install wl-clipboard (Wayland) or xclip (X11) and run the host daemon in your desktop session")]
    Unavailable,
    /// Several files are copied and at least one is not a usable image.
    #[error("every copied file must be a PNG or JPEG image matching the requested format")]
    MixedSelection,
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

/// Read every PNG or JPEG representation from the host's desktop clipboard.
///
/// On macOS, local file URLs take precedence over rendered clipboard
/// representations so copying PNG/JPEG files in Finder transfers their contents
/// rather than Finder's icons. Copying several files yields one image each, in
/// pasteboard order, and every file must match the requested format. Otherwise
/// automatic selection prefers PNG. Explicit formats require that
/// representation to be supplied by the clipboard. PNG also supports converting
/// a TIFF-only clipboard image. No clipboard content, file paths, or helper
/// diagnostics are logged.
///
/// # Errors
///
/// Returns an error when no requested image is available, the selection mixes
/// images with other files, the desktop clipboard cannot be reached, or the
/// helper exceeds its time, count, or output limit.
pub async fn capture(format: ClipboardFormat) -> Result<Vec<ClipboardImage>, CaptureError> {
    #[cfg(target_os = "macos")]
    return capture_from_pasteboard(format, None).await;

    #[cfg(not(target_os = "macos"))]
    capture_candidates(format).await
}

#[cfg(not(target_os = "macos"))]
async fn capture_candidates(format: ClipboardFormat) -> Result<Vec<ClipboardImage>, CaptureError> {
    let formats: &[ClipboardFormat] = match format {
        ClipboardFormat::Auto => &[ClipboardFormat::Png, ClipboardFormat::Jpeg],
        ClipboardFormat::Png => &[ClipboardFormat::Png],
        ClipboardFormat::Jpeg => &[ClipboardFormat::Jpeg],
    };
    for &candidate in formats {
        match capture_format(candidate).await {
            Ok(bytes) if matches_format(&bytes, candidate) => {
                return Ok(vec![ClipboardImage {
                    format: candidate,
                    bytes,
                }])
            }
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
fn image_format(bytes: &[u8], requested: ClipboardFormat) -> Option<ClipboardFormat> {
    [ClipboardFormat::Png, ClipboardFormat::Jpeg]
        .into_iter()
        .find(|&candidate| {
            matches_format(bytes, candidate)
                && (requested == ClipboardFormat::Auto || requested == candidate)
        })
}

#[cfg(target_os = "macos")]
async fn capture_from_pasteboard(
    format: ClipboardFormat,
    pasteboard_name: Option<&str>,
) -> Result<Vec<ClipboardImage>, CaptureError> {
    let native_types = match format {
        ClipboardFormat::Png => "['public.png']",
        ClipboardFormat::Jpeg => "['public.jpeg']",
        ClipboardFormat::Auto => "['public.png', 'public.jpeg']",
    };
    let allow_tiff = !matches!(format, ClipboardFormat::Jpeg);
    // Only fixed enum-derived values enter this script. Writing NSData directly
    // avoids text encodings, legacy AppleScript clipboard classes and temp files.
    // Payloads are concatenated after a JSON size header because one pipe
    // carries every image of a multi-file selection.
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
    const payloads = [];
    // JXA exposes NSUInteger properties as strings on some macOS versions.
    const fileUrlCount = Number(fileUrls.count);

    if (fileUrlCount > 0) {{
        if (fileUrlCount > {MAX_BATCH_IMAGES}) throw new Error('Too many image files');
        for (let i = 0; i < fileUrlCount; i++) {{
            const handle = $.NSFileHandle.fileHandleForReadingAtPath(
                fileUrls.objectAtIndex(i).path);
            if (handle.isNil()) throw new Error('Image file is unavailable');
            payloads.push(handle.readDataOfLength({MAX_IMAGE_BYTES_PLUS_ONE}));
        }}
        if (board.changeCount !== changeCount) throw new Error('Clipboard changed');
    }} else {{
        let data = $();
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
        payloads.push(data);
    }}
    const sizes = [];
    let total = 0;
    for (let i = 0; i < payloads.length; i++) {{
        if (payloads[i].isNil()) throw new Error('No requested image');
        const length = Number(payloads[i].length);
        if (length > {MAX_IMAGE_BYTES}) throw new Error('Image too large');
        total += length;
        sizes.push(length);
    }}
    if (total > {MAX_BATCH_BYTES}) throw new Error('Images too large');
    const out = $.NSFileHandle.fileHandleWithStandardOutput;
    out.writeData($(JSON.stringify({{sizes: sizes}}) + '\n')
        .dataUsingEncoding($.NSUTF8StringEncoding));
    for (let i = 0; i < payloads.length; i++) out.writeData(payloads[i]);
}}"#,
        MAX_IMAGE_BYTES_PLUS_ONE = MAX_IMAGE_BYTES + 1
    );
    let mut command = Command::new("/usr/bin/osascript");
    command.args(["-l", "JavaScript", "-e", &script]);
    if let Some(name) = pasteboard_name {
        command.arg(name);
    }
    let output = run_helper(
        &mut command,
        MAX_HEADER_BYTES + MAX_BATCH_BYTES,
        CAPTURE_TIMEOUT,
    )
    .await?;
    split_captured_images(&output, format)
}

#[cfg(target_os = "macos")]
fn split_captured_images(
    output: &[u8],
    format: ClipboardFormat,
) -> Result<Vec<ClipboardImage>, CaptureError> {
    let newline = output
        .iter()
        .take(MAX_HEADER_BYTES)
        .position(|&byte| byte == b'\n')
        .ok_or(CaptureError::Unavailable)?;
    let header: CaptureHeader =
        serde_json::from_slice(&output[..newline]).map_err(|_| CaptureError::Unavailable)?;
    if header.sizes.is_empty() || header.sizes.len() > MAX_BATCH_IMAGES {
        return Err(CaptureError::Unavailable);
    }
    let count = header.sizes.len();
    let mut rest = &output[newline + 1..];
    let mut images = Vec::with_capacity(count);
    for size in header.sizes {
        if size == 0 || size > MAX_IMAGE_BYTES as u64 || size > rest.len() as u64 {
            return Err(CaptureError::Unavailable);
        }
        let (bytes, tail) = rest.split_at(size as usize);
        rest = tail;
        let Some(matched) = image_format(bytes, format) else {
            return Err(if count > 1 {
                CaptureError::MixedSelection
            } else {
                CaptureError::Unavailable
            });
        };
        images.push(ClipboardImage {
            format: matched,
            bytes: bytes.to_vec(),
        });
    }
    if !rest.is_empty() {
        return Err(CaptureError::Unavailable);
    }
    Ok(images)
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
    fn pasteboard_name() -> String {
        format!("devcontainer-bridge-tests-{}", uuid::Uuid::new_v4())
    }

    #[cfg(target_os = "macos")]
    fn only(images: Vec<ClipboardImage>) -> Vec<u8> {
        let [image] = <[ClipboardImage; 1]>::try_from(images).expect("expected one image");
        image.bytes
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
            let name = pasteboard_name();
            seed_file_pasteboard(&name, &[source.as_path()], &icon)
                .await
                .unwrap();
            assert_eq!(
                only(capture_from_pasteboard(format, Some(&name)).await.unwrap()),
                expected
            );
        }

        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[unsupported.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));

        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[jpeg.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Png, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));

        let oversized = root.path().join("oversized.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[oversized.as_path()], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn finder_multi_file_selection_transfers_every_image_in_order() {
        let root = tempfile::tempdir().unwrap();
        let icon = root.path().join("finder-icon.png");
        std::fs::write(&icon, b"\x89PNG\r\n\x1a\nfinder-icon").unwrap();
        let first = root.path().join("first.png");
        let second = root.path().join("second.jpg");
        let third = root.path().join("third.png");
        let unsupported = root.path().join("notes.txt");
        let first_bytes = b"\x89PNG\r\n\x1a\nfirst".as_slice();
        let second_bytes = b"\xff\xd8\xffsecond".as_slice();
        let third_bytes = b"\x89PNG\r\n\x1a\nthird".as_slice();
        std::fs::write(&first, first_bytes).unwrap();
        std::fs::write(&second, second_bytes).unwrap();
        std::fs::write(&third, third_bytes).unwrap();
        std::fs::write(&unsupported, b"not an image").unwrap();

        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[&first, &second, &third], &icon)
            .await
            .unwrap();
        assert_eq!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name))
                .await
                .unwrap(),
            vec![
                ClipboardImage {
                    format: ClipboardFormat::Png,
                    bytes: first_bytes.to_vec(),
                },
                ClipboardImage {
                    format: ClipboardFormat::Jpeg,
                    bytes: second_bytes.to_vec(),
                },
                ClipboardImage {
                    format: ClipboardFormat::Png,
                    bytes: third_bytes.to_vec(),
                },
            ]
        );

        // An explicit format applies to every file, not just the first.
        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[&first, &third], &icon)
            .await
            .unwrap();
        assert_eq!(
            capture_from_pasteboard(ClipboardFormat::Png, Some(&name))
                .await
                .unwrap()
                .len(),
            2
        );
        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[&first, &second], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Png, Some(&name)).await,
            Err(CaptureError::MixedSelection)
        ));

        let name = pasteboard_name();
        seed_file_pasteboard(&name, &[&first, &unsupported], &icon)
            .await
            .unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::MixedSelection)
        ));

        let extras: Vec<std::path::PathBuf> = (0..=MAX_BATCH_IMAGES)
            .map(|index| {
                let path = root.path().join(format!("extra-{index}.png"));
                std::fs::write(&path, first_bytes).unwrap();
                path
            })
            .collect();
        let sources: Vec<&Path> = extras.iter().map(std::path::PathBuf::as_path).collect();
        let name = pasteboard_name();
        seed_file_pasteboard(&name, &sources, &icon).await.unwrap();
        assert!(matches!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name)).await,
            Err(CaptureError::Unavailable)
        ));
        let name = pasteboard_name();
        seed_file_pasteboard(&name, &sources[1..], &icon)
            .await
            .unwrap();
        assert_eq!(
            capture_from_pasteboard(ClipboardFormat::Auto, Some(&name))
                .await
                .unwrap()
                .len(),
            MAX_BATCH_IMAGES
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_output_must_match_its_size_header() {
        let png = b"\x89PNG\r\n\x1a\n".as_slice();
        let framed = |header: &str, payload: &[u8]| {
            let mut bytes = header.as_bytes().to_vec();
            bytes.push(b'\n');
            bytes.extend_from_slice(payload);
            bytes
        };
        assert_eq!(
            split_captured_images(
                &framed(r#"{"sizes":[8,8]}"#, &[png, png].concat()),
                ClipboardFormat::Auto
            )
            .unwrap()
            .len(),
            2
        );
        for output in [
            framed(r#"{"sizes":[]}"#, b""),
            framed(r#"{"sizes":[8]}"#, b"\x89PNG\r\n"),
            framed(r#"{"sizes":[0]}"#, b""),
            framed(r#"{"sizes":[8]}"#, &[png, b"extra".as_slice()].concat()),
            framed(&format!(r#"{{"sizes":[{}]}}"#, MAX_IMAGE_BYTES + 1), png),
            framed("not json", png),
            png.to_vec(),
        ] {
            assert!(split_captured_images(&output, ClipboardFormat::Auto).is_err());
        }
        let oversized: Vec<u64> = vec![8; MAX_BATCH_IMAGES + 1];
        assert!(split_captured_images(
            &framed(
                &format!(r#"{{"sizes":{oversized:?}}}"#),
                &png.repeat(MAX_BATCH_IMAGES + 1)
            ),
            ClipboardFormat::Auto
        )
        .is_err());
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
