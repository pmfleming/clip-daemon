//! Capture lifecycle contracts. Publication has an independent lifetime.
use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use async_trait::async_trait;

mod policy;
mod transfer;
mod wayland;
mod worker;
pub use worker::{CaptureSink, Controller};

/// Implementations acknowledge pause only after fencing every storage submission.
#[async_trait]
pub trait CaptureControl: Send + Sync {
    async fn set_paused(&self, paused: bool, max_bytes: u64) -> Result<(), String>;
    async fn is_paused(&self) -> Result<bool, String>;

    async fn shutdown(&self) -> Result<(), String> {
        self.set_paused(true, 0).await
    }
}

/// API-only constructors must never start an external collector as a side effect.
pub(crate) struct Unavailable;

#[async_trait]
impl CaptureControl for Unavailable {
    async fn set_paused(&self, paused: bool, _: u64) -> Result<(), String> {
        if paused {
            Ok(())
        } else {
            Err("No capture owner is attached to this API".into())
        }
    }
    async fn is_paused(&self) -> Result<bool, String> {
        Ok(true)
    }
}

/// Generation carried by an offer from admission through storage submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u64);

/// Admission starts closed. Invalidation is immediate even while IPC is blocked;
/// the serialized fence is acknowledged only after an already-sent request ends.
#[derive(Default)]
pub struct Admission {
    open: AtomicBool,
    generation: AtomicU64,
    submission: Mutex<()>,
    uncertain: AtomicBool,
}

impl Admission {
    pub fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Call off the async executor: this waits for any submitted IPC request.
    pub fn fence(&self) -> Result<(), String> {
        self.verified_submission().map(drop)
    }

    pub fn resume(&self) -> Result<(), String> {
        let _guard = self.verified_submission()?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.open.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn verified_submission(&self) -> Result<MutexGuard<'_, ()>, String> {
        let guard = self
            .submission
            .lock()
            .map_err(|_| "Capture sink panicked")?;
        if self.uncertain.load(Ordering::SeqCst) {
            return Err(
                "Capture submission outcome is uncertain; engine recovery is required".into(),
            );
        }
        Ok(guard)
    }

    pub fn admit(&self) -> Option<Generation> {
        let generation = self.generation.load(Ordering::SeqCst);
        self.open
            .load(Ordering::SeqCst)
            .then_some(Generation(generation))
    }

    pub fn accepts(&self, generation: Generation) -> bool {
        self.open.load(Ordering::SeqCst) && generation.0 == self.generation.load(Ordering::SeqCst)
    }

    /// `Err` means uncertain persistence, not a safe rejection. Such a result
    /// permanently closes this generation's controller; never blindly retry Add.
    pub fn submit<T>(
        &self,
        generation: Generation,
        submit: impl FnOnce() -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let _guard = self
            .submission
            .lock()
            .map_err(|_| "Capture sink panicked")?;
        if !self.accepts(generation) {
            return Ok(None);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(submit))
            .map_err(|_| "Capture sink panicked".to_owned())
            .and_then(|result| result);
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                self.uncertain.store(true, Ordering::SeqCst);
                self.close();
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use super::Admission;

    #[test]
    fn admission_is_closed_initially_and_old_transfers_never_revive() {
        let gate = Admission::default();
        assert!(gate.admit().is_none());
        gate.resume().unwrap();
        let old = gate.admit().unwrap();
        gate.close();
        gate.fence().unwrap();
        gate.resume().unwrap();
        assert!(!gate.accepts(old));
        assert_eq!(
            gate.submit(old, || panic!("stale transfer reached sink"))
                .unwrap(),
            None::<()>
        );
        assert_eq!(
            gate.submit(gate.admit().unwrap(), || Ok(7)).unwrap(),
            Some(7)
        );
    }

    #[test]
    fn pause_fences_in_flight_submission_and_rejects_queued_transfers() {
        let gate = Arc::new(Admission::default());
        gate.resume().unwrap();
        let generation = gate.admit().unwrap();
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let sink_gate = gate.clone();
        let sink = std::thread::spawn(move || {
            sink_gate.submit(generation, || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
        });
        started.recv().unwrap();
        gate.close();
        assert!(gate.admit().is_none());
        let (finished, done) = mpsc::channel();
        let pause_gate = gate.clone();
        let pause = std::thread::spawn(move || {
            pause_gate.fence().unwrap();
            finished.send(()).unwrap();
        });
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        sink.join().unwrap().unwrap();
        pause.join().unwrap();
        done.recv().unwrap();
        assert_eq!(
            gate.submit(generation, || panic!("queued transfer reached sink"))
                .unwrap(),
            None::<()>
        );
    }

    #[test]
    fn sink_panic_closes_admission_without_poisoning_the_fence() {
        let gate = Admission::default();
        gate.resume().unwrap();
        assert!(
            gate.submit::<()>(gate.admit().unwrap(), || panic!("injected"))
                .is_err()
        );
        assert!(gate.admit().is_none());
        assert!(gate.fence().is_err());
    }

    #[test]
    fn ambiguous_submission_never_becomes_verified_pause_or_retry() {
        let gate = Admission::default();
        gate.resume().unwrap();
        assert!(
            gate.submit::<()>(gate.admit().unwrap(), || Err("lost reply".into()))
                .is_err()
        );
        assert!(gate.admit().is_none());
        assert!(gate.fence().is_err());
        assert!(gate.resume().is_err());
    }
}
