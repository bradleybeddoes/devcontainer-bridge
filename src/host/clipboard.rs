//! Authenticated, opt-in clipboard transfers on dedicated data connections.

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::clipboard_capture::{self, ClipboardImage};
use crate::control::{self, ControlError};
use crate::protocol::{ClipboardFormat, ClipboardToken, Message};

/// Clipboard policy and concurrency limit shared by data connections.
pub(super) struct ClipboardService {
    enabled: bool,
    token: Option<String>,
    transfer: Semaphore,
}

impl ClipboardService {
    pub(super) fn new(enabled: bool, token: Option<String>) -> Self {
        Self {
            enabled,
            token,
            transfer: Semaphore::new(1),
        }
    }

    /// Authenticate before touching the clipboard, then stream bounded images.
    pub(super) async fn serve<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        format: ClipboardFormat,
        token: ClipboardToken,
    ) -> Result<(), ControlError> {
        if !self.enabled {
            return reject(
                writer,
                "clipboard sharing is disabled; start host-daemon with --allow-clipboard",
            )
            .await;
        }
        if !self
            .token
            .as_ref()
            .is_some_and(|expected| !expected.is_empty() && expected == &token.0)
        {
            return reject(writer, "clipboard authentication failed").await;
        }
        let Ok(_permit) = self.transfer.try_acquire() else {
            return reject(
                writer,
                "another clipboard transfer is in progress; try again",
            )
            .await;
        };
        let images = match clipboard_capture::capture(format).await {
            Ok(images) => images,
            Err(error) => return reject(writer, &error.to_string()).await,
        };
        write_images(writer, &images).await?;
        writer.shutdown().await?;
        Ok(())
    }
}

/// Stream captured images, announcing the count only for multi-image selections.
///
/// A single image keeps the original one-header response so clients released
/// before batch support continue to read it.
async fn write_images<W: AsyncWrite + Unpin>(
    writer: &mut W,
    images: &[ClipboardImage],
) -> Result<(), ControlError> {
    if images.len() > 1 {
        control::write_message(
            writer,
            &Message::ClipboardBatchReady {
                count: images.len() as u32,
            },
        )
        .await?;
    }
    for image in images {
        control::write_message(
            writer,
            &Message::ClipboardReady {
                format: image.format,
                size: image.bytes.len() as u64,
            },
        )
        .await?;
        writer.write_all(&image.bytes).await?;
    }
    Ok(())
}

async fn reject<W: AsyncWrite + Unpin>(writer: &mut W, error: &str) -> Result<(), ControlError> {
    control::write_message(
        writer,
        &Message::ClipboardError {
            error: error.into(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_require_opt_in_and_a_nonempty_matching_token() {
        for (enabled, expected, provided, reason) in [
            (false, Some("secret"), "secret", "disabled"),
            (true, None, "", "authentication failed"),
            (true, Some(""), "", "authentication failed"),
            (true, Some("secret"), "wrong", "authentication failed"),
        ] {
            let service = ClipboardService::new(enabled, expected.map(String::from));
            let mut output = Vec::new();
            service
                .serve(
                    &mut output,
                    ClipboardFormat::Auto,
                    ClipboardToken(provided.into()),
                )
                .await
                .unwrap();
            let Message::ClipboardError { error } =
                control::read_message(&mut &output[..]).await.unwrap()
            else {
                panic!("unauthorized request was not rejected");
            };
            assert!(error.contains(reason));
        }
    }

    #[tokio::test]
    async fn only_multi_image_responses_announce_a_count() {
        let png = ClipboardImage {
            format: ClipboardFormat::Png,
            bytes: b"\x89PNG\r\n\x1a\nfirst".to_vec(),
        };
        let jpeg = ClipboardImage {
            format: ClipboardFormat::Jpeg,
            bytes: b"\xff\xd8\xffsecond".to_vec(),
        };

        let mut output = Vec::new();
        write_images(&mut output, std::slice::from_ref(&png))
            .await
            .unwrap();
        let mut reader = &output[..];
        assert_eq!(
            control::read_message(&mut reader).await.unwrap(),
            Message::ClipboardReady {
                format: ClipboardFormat::Png,
                size: png.bytes.len() as u64,
            }
        );
        assert_eq!(reader, png.bytes);

        let mut output = Vec::new();
        write_images(&mut output, &[png.clone(), jpeg.clone()])
            .await
            .unwrap();
        let mut reader = &output[..];
        assert_eq!(
            control::read_message(&mut reader).await.unwrap(),
            Message::ClipboardBatchReady { count: 2 }
        );
        for image in [&png, &jpeg] {
            assert_eq!(
                control::read_message(&mut reader).await.unwrap(),
                Message::ClipboardReady {
                    format: image.format,
                    size: image.bytes.len() as u64,
                }
            );
            let (bytes, rest) = reader.split_at(image.bytes.len());
            assert_eq!(bytes, image.bytes);
            reader = rest;
        }
        assert!(reader.is_empty());
    }

    #[tokio::test]
    async fn concurrent_transfer_is_rejected_without_reading_clipboard() {
        let service = ClipboardService::new(true, Some("secret".into()));
        let _permit = service.transfer.acquire().await.unwrap();
        let mut output = Vec::new();
        service
            .serve(
                &mut output,
                ClipboardFormat::Png,
                ClipboardToken("secret".into()),
            )
            .await
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("in progress"));
    }
}
