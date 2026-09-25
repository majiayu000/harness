//! Incremental bounded stdout frame reader for Codex app-server (#2095 §3.4).
//!
//! Replaces `Lines<BufReader<_>>` so a single JSON-RPC frame cannot grow without
//! bound before parsing. Private to this adapter; not a generic codec.

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Maximum encoded JSON-RPC frame size for Codex app-server stdout.
///
/// Counts frame bytes **excluding** the terminating LF and **including** an
/// optional CR that precedes it (CRLF). This is an engineering default for
/// memory bounding (#2095 §3.4), not a provider protocol guarantee.
///
/// Representative captured valid app-server payload fixtures were not present
/// in-tree when this default was chosen; revisit if real captures show larger
/// legitimate frames.
pub(super) const DEFAULT_MAX_PROTOCOL_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Incremental reader that rejects frames exceeding `max_frame_bytes`.
///
/// Capacity is checked before each append. Oversized input is rejected with or
/// without a newline; the reader never truncates a frame or continues parsing
/// its suffix. Partial frame bytes remain in `pending` across successful
/// chunked reads. Cancellation of the owning adapter session is terminal
/// (existing cleanup path); a cancelled mid-frame read does not silently drop
/// bytes into a reused session because the reader is discarded with the child.
pub(super) struct BoundedStdoutReader<R> {
    inner: BufReader<R>,
    pending: Vec<u8>,
    max_frame_bytes: usize,
}

impl<R: AsyncRead + Unpin> BoundedStdoutReader<R> {
    pub(super) fn new(inner: BufReader<R>, max_frame_bytes: usize) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            max_frame_bytes,
        }
    }

    pub(super) fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Read the next protocol frame (newline-delimited), stripping LF and an
    /// optional preceding CR. Returns `Ok(None)` on clean EOF with no pending
    /// bytes; returns a final frame without a trailing newline on EOF with
    /// pending data (same as tokio `Lines`).
    pub(super) async fn next_frame(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return self.take_pending_frame();
            }

            if let Some(newline_at) = available.iter().position(|&b| b == b'\n') {
                let before_lf = newline_at;
                let total_before_lf = self
                    .pending
                    .len()
                    .checked_add(before_lf)
                    .ok_or_else(frame_too_large_io)?;
                if total_before_lf > self.max_frame_bytes {
                    return Err(frame_too_large_io());
                }
                self.pending.extend_from_slice(&available[..before_lf]);
                self.inner.consume(before_lf + 1);
                return self.take_pending_frame();
            }

            let incoming = available.len();
            let total = self
                .pending
                .len()
                .checked_add(incoming)
                .ok_or_else(frame_too_large_io)?;
            if total > self.max_frame_bytes {
                // Reject immediately without a newline; do not append or scan
                // for a later delimiter that could yield a truncated frame.
                return Err(frame_too_large_io());
            }
            self.pending.extend_from_slice(available);
            self.inner.consume(incoming);
        }
    }

    fn take_pending_frame(&mut self) -> std::io::Result<Option<String>> {
        if self.pending.last() == Some(&b'\r') {
            self.pending.pop();
        }
        let bytes = std::mem::take(&mut self.pending);
        let line = String::from_utf8(bytes).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.utf8_error())
        })?;
        Ok(Some(line))
    }
}

fn frame_too_large_io() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "codex app-server protocol frame exceeds maximum size",
    )
}

pub(super) fn protocol_frame_too_large_error(
    max_frame_bytes: usize,
) -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(format!(
        "codex app-server protocol frame exceeds maximum size ({max_frame_bytes} bytes, excluding LF)"
    ))
}

pub(super) fn map_frame_read_error(
    error: std::io::Error,
    max_frame_bytes: usize,
) -> harness_core::error::HarnessError {
    if error.kind() == std::io::ErrorKind::InvalidData
        && error
            .to_string()
            .contains("protocol frame exceeds maximum size")
    {
        return protocol_frame_too_large_error(max_frame_bytes);
    }
    harness_core::error::HarnessError::AgentExecution(format!(
        "failed reading codex app-server stdout: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    async fn reader_over(data: &[u8], max: usize) -> BoundedStdoutReader<Cursor<Vec<u8>>> {
        BoundedStdoutReader::new(BufReader::new(Cursor::new(data.to_vec())), max)
    }

    #[tokio::test]
    async fn accepts_frame_exactly_at_limit_with_lf() {
        let payload = vec![b'a'; 16];
        let mut data = payload.clone();
        data.push(b'\n');
        let mut reader = reader_over(&data, 16).await;
        let frame = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(frame.as_bytes(), payload.as_slice());
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accepts_crlf_when_cr_fits_in_limit() {
        // 15 payload bytes + CR = 16 counted; LF excluded.
        let mut data = vec![b'b'; 15];
        data.extend_from_slice(b"\r\n");
        let mut reader = reader_over(&data, 16).await;
        let frame = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(frame, "b".repeat(15));
    }

    #[tokio::test]
    async fn rejects_crlf_when_cr_pushes_over_limit() {
        // 16 payload bytes + CR = 17 > 16.
        let mut data = vec![b'c'; 16];
        data.extend_from_slice(b"\r\n");
        let mut reader = reader_over(&data, 16).await;
        let err = reader.next_frame().await.expect_err("CR must count");
        assert!(err.to_string().contains("exceeds maximum size"));
    }

    #[tokio::test]
    async fn rejects_oversize_with_newline() {
        let mut data = vec![b'd'; 17];
        data.push(b'\n');
        let mut reader = reader_over(&data, 16).await;
        let err = reader.next_frame().await.expect_err("oversize with LF");
        assert!(err.to_string().contains("exceeds maximum size"));
    }

    #[tokio::test]
    async fn rejects_oversize_without_newline() {
        let data = vec![b'e'; 17];
        let mut reader = reader_over(&data, 16).await;
        let err = reader.next_frame().await.expect_err("oversize without LF");
        assert!(err.to_string().contains("exceeds maximum size"));
    }

    #[tokio::test]
    async fn does_not_parse_suffix_after_oversize_prefix() {
        // Oversize prefix then a newline and a valid-looking JSON line.
        let mut data = vec![b'x'; 20];
        data.push(b'\n');
        data.extend_from_slice(br#"{"id":1,"result":{}}"#);
        data.push(b'\n');
        let mut reader = reader_over(&data, 16).await;
        let err = reader.next_frame().await.expect_err("must reject oversize");
        assert!(err.to_string().contains("exceeds maximum size"));
        // Reader must not yield the suffix as a subsequent frame after error;
        // session cleanup discards the reader. Confirm pending was not swapped
        // into a decoded frame by ensuring a fresh read still errors or EOFs
        // without returning the JSON object.
        match reader.next_frame().await {
            Err(_) | Ok(None) => {}
            Ok(Some(frame)) => {
                assert!(
                    !frame.contains("\"result\""),
                    "must not decode suffix after oversize: {frame}"
                );
            }
        }
    }

    #[tokio::test]
    async fn preserves_blank_line_and_eof_without_newline() {
        let mut reader = reader_over(b"\nfinal", 64).await;
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), "");
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), "final");
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_invalid_utf8() {
        let mut reader = reader_over(&[0xff, 0xfe, b'\n'], 64).await;
        let err = reader.next_frame().await.expect_err("invalid utf-8");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn default_limit_boundary_accepts_exact_8_mib() {
        let max = DEFAULT_MAX_PROTOCOL_FRAME_BYTES;
        // Avoid allocating 8 MiB twice: build stream as chunked cursor content.
        let mut data = vec![b'z'; max];
        data.push(b'\n');
        let mut reader = BoundedStdoutReader::new(BufReader::new(Cursor::new(data)), max);
        let frame = reader
            .next_frame()
            .await
            .expect("exact default limit must be valid")
            .expect("frame present");
        assert_eq!(frame.len(), max);
    }

    #[tokio::test]
    async fn default_limit_boundary_rejects_one_over_8_mib() {
        let max = DEFAULT_MAX_PROTOCOL_FRAME_BYTES;
        let mut data = vec![b'z'; max + 1];
        data.push(b'\n');
        let mut reader = BoundedStdoutReader::new(BufReader::new(Cursor::new(data)), max);
        let err = reader
            .next_frame()
            .await
            .expect_err("one byte over default limit must fail");
        assert!(err.to_string().contains("exceeds maximum size"));
    }
}
