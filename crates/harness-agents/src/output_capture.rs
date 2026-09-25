use std::collections::VecDeque;

/// Fixed-capacity byte buffer that keeps the most recent bytes pushed into it.
struct TailBuffer {
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl TailBuffer {
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            self.truncated = self.truncated || !self.data.is_empty() || chunk.len() > self.cap;
            self.data.clear();
            self.data
                .extend_from_slice(&chunk[chunk.len() - self.cap..]);
            return;
        }
        let overflow = (self.data.len() + chunk.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.data.drain(..overflow);
            self.truncated = true;
        }
        self.data.extend_from_slice(chunk);
    }
}

/// Redacts configured byte sequences before retaining the bounded output tail.
///
/// The pending prefix bridges arbitrary read boundaries. This is essential for
/// secrets: truncating raw output first can discard the beginning of a secret
/// and leave an unrecognizable suffix in the retained tail.
pub(super) struct OutputCapture {
    tail: TailBuffer,
    secrets: Vec<Vec<u8>>,
    pending: VecDeque<u8>,
}

impl OutputCapture {
    pub(super) fn new(cap: usize, secret_values: &[String]) -> Self {
        let mut secrets: Vec<Vec<u8>> = secret_values
            .iter()
            .filter(|secret| !secret.is_empty())
            .map(|secret| secret.as_bytes().to_vec())
            .collect();
        secrets.sort_unstable_by(|left, right| {
            right.len().cmp(&left.len()).then_with(|| left.cmp(right))
        });
        secrets.dedup();
        Self {
            tail: TailBuffer::new(cap),
            secrets,
            pending: VecDeque::new(),
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) {
        if self.secrets.is_empty() {
            self.tail.push(chunk);
            return;
        }
        self.pending.extend(chunk);
        self.emit_available(false);
    }

    pub(super) fn finish(&mut self) {
        self.emit_available(true);
    }

    pub(super) fn truncated(&self) -> bool {
        self.tail.truncated
    }

    pub(super) fn into_data(self) -> Vec<u8> {
        self.tail.data
    }

    fn emit_available(&mut self, finishing: bool) {
        let mut emitted = Vec::new();
        while !self.pending.is_empty() {
            let full_match_len = self
                .secrets
                .iter()
                .find(|secret| self.pending_starts_with(secret))
                .map(Vec::len);
            let partial_match = self.secrets.iter().any(|secret| {
                self.pending.len() < secret.len() && self.secret_starts_with_pending(secret)
            });

            if partial_match {
                if finishing {
                    self.pending.clear();
                    emitted.extend_from_slice(b"***");
                }
                break;
            }
            if let Some(secret_len) = full_match_len {
                self.pending.drain(..secret_len);
                emitted.extend_from_slice(b"***");
            } else if let Some(byte) = self.pending.pop_front() {
                emitted.push(byte);
            }
        }
        self.tail.push(&emitted);
    }

    fn pending_starts_with(&self, candidate: &[u8]) -> bool {
        candidate.len() <= self.pending.len()
            && self
                .pending
                .iter()
                .take(candidate.len())
                .copied()
                .eq(candidate.iter().copied())
    }

    fn secret_starts_with_pending(&self, secret: &[u8]) -> bool {
        self.pending
            .iter()
            .copied()
            .eq(secret.iter().take(self.pending.len()).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_buffer_keeps_only_most_recent_bytes() {
        let mut buf = TailBuffer::new(4);
        buf.push(b"ab");
        assert_eq!(buf.data, b"ab");
        assert!(!buf.truncated);
        buf.push(b"cdef");
        assert_eq!(buf.data, b"cdef");
        assert!(buf.truncated);

        let mut big = TailBuffer::new(4);
        big.push(b"0123456789");
        assert_eq!(big.data, b"6789");
        assert!(big.truncated);
    }

    #[test]
    fn output_capture_redacts_across_read_boundaries_before_truncation() {
        let secrets = ["TOP-SECRET-TOKEN".to_string()];
        let mut capture = OutputCapture::new(12, &secrets);

        capture.push(b"prefix-TOP-SEC");
        capture.push(b"RET-TOKEN-tail");
        capture.finish();

        assert_eq!(capture.into_data(), b"fix-***-tail");
    }

    #[test]
    fn output_capture_redacts_secret_longer_than_capture_limit() {
        let secret = "s".repeat(8192);
        let mut capture = OutputCapture::new(4096, std::slice::from_ref(&secret));

        for chunk in secret.as_bytes().chunks(257) {
            capture.push(chunk);
        }
        capture.push(b"-tail");
        capture.finish();

        assert_eq!(capture.into_data(), b"***-tail");
    }

    #[test]
    fn output_capture_redacts_secret_prefix_at_end_of_stream() {
        let secrets = ["secret-value".to_string()];
        let mut capture = OutputCapture::new(64, &secrets);

        capture.push(b"failure: secret-");
        capture.finish();

        assert_eq!(capture.into_data(), b"failure: ***");
    }
}
