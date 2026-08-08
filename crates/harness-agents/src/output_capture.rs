/// Fixed-capacity byte buffer that keeps the most recent bytes pushed into it.
pub(super) struct TailBuffer {
    pub(super) data: Vec<u8>,
    cap: usize,
    pub(super) truncated: bool,
}

impl TailBuffer {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    pub(super) fn push(&mut self, chunk: &[u8]) {
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
}
