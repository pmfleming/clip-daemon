use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::Notify;

const EDITING: u8 = 0;
const COMMITTING: u8 = 1;
const CANCELLED: u8 = 2;
const FINISHED: u8 = 3;

#[derive(Default)]
pub(super) struct OperationControl {
    phase: AtomicU8,
    finished: Notify,
}

impl OperationControl {
    pub fn begin_commit(&self) -> bool {
        self.phase
            .compare_exchange(EDITING, COMMITTING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(EDITING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish(&self) {
        self.phase.store(FINISHED, Ordering::SeqCst);
        self.finished.notify_waiters();
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.phase.load(Ordering::SeqCst) == FINISHED {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OperationControl;

    #[tokio::test]
    async fn cancellation_and_commit_are_mutually_exclusive() {
        for commit_first in [false, true] {
            let control = OperationControl::default();
            if commit_first {
                assert!(control.begin_commit());
                assert!(!control.cancel());
            } else {
                assert!(control.cancel());
                assert!(!control.begin_commit());
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(5), control.wait())
                    .await
                    .is_err()
            );
            control.finish();
            control.wait().await;
        }
    }
}
