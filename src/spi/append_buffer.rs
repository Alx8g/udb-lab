//! Writer-private bounded append bytes. Drop never performs I/O.

pub(crate) const APPEND_CAPACITY: usize = 64 * 1024;

pub(crate) struct AppendBuffer {
    start: u64,
    bytes: Vec<u8>,
}

impl AppendBuffer {
    pub(crate) fn new(start: u64, enabled: bool) -> Self {
        Self {
            start,
            bytes: Vec::with_capacity(if enabled { APPEND_CAPACITY } else { 0 }),
        }
    }
    pub(crate) fn start(&self) -> u64 {
        self.start
    }
    pub(crate) fn remaining(&self) -> usize {
        APPEND_CAPACITY.min(self.bytes.capacity()) - self.bytes.len()
    }
    pub(crate) fn pending(&self, at: u64) -> Option<&[u8]> {
        let offset = usize::try_from(at.checked_sub(self.start)?).ok()?;
        if offset >= self.bytes.len() {
            return None;
        }
        Some(&self.bytes[offset..])
    }
    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.remaining());
        self.bytes.extend_from_slice(bytes);
    }
    pub(crate) fn flush<E>(
        &mut self,
        write: impl FnOnce(u64, &[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        write(self.start, &self.bytes)?;
        self.start += self.bytes.len() as u64;
        self.bytes.clear();
        Ok(())
    }
    pub(crate) fn advance_direct(&mut self, end: u64) {
        assert!(self.bytes.is_empty() && end >= self.start);
        self.start = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_flush_does_not_advance_or_discard_pending_bytes() {
        let mut buffer = AppendBuffer::new(8, true);
        buffer.extend(b"node");
        assert!(buffer.flush(|_, _| Err::<(), _>("partial write")).is_err());
        assert_eq!(buffer.pending(8), Some(&b"node"[..]));
        buffer
            .flush::<()>(|at, bytes| {
                assert_eq!(at, 8);
                assert_eq!(bytes, b"node");
                Ok(())
            })
            .unwrap();
        assert_eq!(buffer.remaining(), APPEND_CAPACITY);
        assert_eq!(buffer.pending(12), None);
    }

    #[test]
    fn empty_flush_does_not_emit_io() {
        let mut buffer = AppendBuffer::new(8, false);
        assert_eq!(buffer.remaining(), 0);
        buffer.flush::<()>(|_, _| panic!("unexpected I/O")).unwrap();
        buffer.advance_direct(100_000);
    }

    #[test]
    fn pending_records_exclude_the_logical_end() {
        let mut buffer = AppendBuffer::new(8, true);
        buffer.extend(&vec![17; APPEND_CAPACITY]);
        assert_eq!(buffer.remaining(), 0);
        assert_eq!(buffer.pending(7), None);
        assert_eq!(buffer.pending(8 + APPEND_CAPACITY as u64), None);
        assert_eq!(
            buffer.pending(8 + APPEND_CAPACITY as u64 - 1),
            Some(&[17][..])
        );
    }
}
