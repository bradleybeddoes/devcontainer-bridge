//! Authenticated, opt-in clipboard transfers on dedicated data connections.

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::clipboard_capture;
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

    /// Authenticate before touching the clipboard, then stream a bounded image.
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
        let bytes = match clipboard_capture::capture(format).await {
            Ok(bytes) => bytes,
            Err(error) => return reject(writer, &error.to_string()).await,
        };
        // Capture validates PNG/JPEG signatures. Preserve whichever representation
        // the clipboard supplied when the request was automatic.
        let actual_format = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            ClipboardFormat::Png
        } else {
            ClipboardFormat::Jpeg
        };
        control::write_message(
            writer,
            &Message::ClipboardReady {
                format: actual_format,
                size: bytes.len() as u64,
            },
        )
        .await?;
        writer.write_all(&bytes).await?;
        writer.shutdown().await?;
        Ok(())
    }
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
