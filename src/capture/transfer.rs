use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    os::fd::{AsFd, BorrowedFd},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rustix::fs::{MemfdFlags, memfd_create};

pub(super) const MAX_TRANSFERS: usize = 4;
const TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(5);
const TOTAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Budget(AtomicU64);

pub(super) struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Budget {
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes).filter(|next| *next <= TOTAL_BYTES)
            })
            .ok()
            .map(|_| Reservation {
                budget: self.clone(),
                bytes,
            })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

pub(super) struct Transfer {
    source: File,
    data: File,
    length: u64,
    limit: u64,
    nonblank: bool,
    started: Instant,
    progress: Instant,
    reservation: Reservation,
}

pub(super) enum Received {
    Blank,
    Complete(File, Reservation),
}

impl AsFd for Transfer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.source.as_fd()
    }
}

impl Transfer {
    pub fn new(source: File, limit: u64, reservation: Reservation) -> io::Result<Self> {
        let data = File::from(memfd_create(c"clip-capture", MemfdFlags::CLOEXEC)?);
        Ok(Self {
            source,
            data,
            length: 0,
            limit,
            nonblank: false,
            started: Instant::now(),
            progress: Instant::now(),
            reservation,
        })
    }

    /// One bounded chunk per tick keeps control responsive under a hot producer.
    pub fn receive(&mut self, now: Instant) -> io::Result<Option<bool>> {
        if now.duration_since(self.progress) >= IDLE || now.duration_since(self.started) >= TOTAL {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut bytes = [0; 64 * 1024];
        let remaining = (self.limit + 1 - self.length).min(bytes.len() as u64) as usize;
        match self.source.read(&mut bytes[..remaining]) {
            Ok(0) => Ok(Some(self.nonblank)),
            Ok(count) => {
                self.length += count as u64;
                if self.length > self.limit {
                    return Err(io::ErrorKind::FileTooLarge.into());
                }
                self.progress = now;
                self.nonblank |= bytes[..count]
                    .iter()
                    .any(|byte| !byte.is_ascii_whitespace());
                self.data.write_all(&bytes[..count])?;
                Ok(None)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn finish(mut self, nonblank: bool) -> io::Result<Received> {
        if !nonblank {
            return Ok(Received::Blank);
        }
        self.data.rewind()?;
        Ok(Received::Complete(self.data, self.reservation))
    }
}

#[cfg(test)]
mod tests {
    use super::{Budget, IDLE, Received, TOTAL_BYTES, Transfer};
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::{
        fs::File,
        io::{Seek, Write},
        sync::Arc,
        time::Instant,
    };

    fn source(bytes: &[u8]) -> File {
        let mut file = File::from(memfd_create(c"capture-test", MemfdFlags::CLOEXEC).unwrap());
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        file
    }

    #[test]
    fn limits_blank_content_and_reservations_are_enforced() {
        let budget = Arc::new(Budget::default());
        for (bytes, limit, accepted) in [(b"abcd".as_slice(), 4, true), (b"abcde", 4, false)] {
            let mut transfer =
                Transfer::new(source(bytes), limit, budget.reserve(limit + 1).unwrap()).unwrap();
            let result = transfer.receive(Instant::now());
            assert_eq!(result.is_ok(), accepted);
            if accepted {
                assert_eq!(transfer.receive(Instant::now()).unwrap(), Some(true));
                assert!(matches!(
                    transfer.finish(true).unwrap(),
                    Received::Complete(..)
                ));
            }
        }
        let mut blank = Transfer::new(source(b" \n\t"), 4, budget.reserve(5).unwrap()).unwrap();
        assert_eq!(blank.receive(Instant::now()).unwrap(), None);
        assert_eq!(blank.receive(Instant::now()).unwrap(), Some(false));
        drop(blank);
        let reserved = budget.reserve(TOTAL_BYTES).unwrap();
        assert!(budget.reserve(1).is_none());
        drop(reserved);
        assert!(budget.reserve(TOTAL_BYTES).is_some());
    }

    #[test]
    fn stalled_transfers_expire_and_binary_non_whitespace_is_kept() {
        let budget = Arc::new(Budget::default());
        let mut transfer = Transfer::new(source(b"\0\xff"), 8, budget.reserve(9).unwrap()).unwrap();
        assert_eq!(transfer.receive(Instant::now()).unwrap(), None);
        assert_eq!(transfer.receive(Instant::now()).unwrap(), Some(true));
        assert!(transfer.receive(Instant::now() + IDLE).is_err());
    }
}
