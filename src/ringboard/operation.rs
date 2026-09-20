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
