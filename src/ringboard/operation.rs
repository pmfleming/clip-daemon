//! Shared image-operation lifecycle: registration, cancellation and commit draining.
use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    future::Future,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
};
use tokio::{
    sync::{Notify, broadcast, oneshot},
    task::JoinHandle,
};

use crate::{
    backend::{BackendResult, operation_error},
    editor::ImageEditorCommand,
    model::OperationResult,
};

pub(super) struct Operations {
    editor: ImageEditorCommand,
    active: Mutex<HashMap<String, OperationTask>>,
    events: broadcast::Sender<OperationResult>,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            editor: ImageEditorCommand::configured(),
            active: Mutex::default(),
            events: broadcast::channel(64).0,
        }
    }
}

struct OperationTask {
    action: &'static str,
    control: Arc<OperationControl>,
    handle: JoinHandle<()>,
    files: Vec<PathBuf>,
}

impl Drop for OperationTask {
    fn drop(&mut self) {
        remove_files(&self.files);
    }
}

fn remove_files(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

pub(super) struct OperationCompletion {
    pub status: &'static str,
    pub message: &'static str,
    pub warning: Option<String>,
}

impl Operations {
    pub async fn edit(&self, input: &Path, output: &Path) -> BackendResult<()> {
        self.editor
            .run(input, output)
            .await
            .map_err(operation_error)
    }

    fn active(&self) -> BackendResult<MutexGuard<'_, HashMap<String, OperationTask>>> {
        self.active
            .lock()
            .map_err(|_| operation_error("Clipboard operation state is unavailable"))
    }

    pub fn active_ids(&self) -> BackendResult<std::collections::HashSet<String>> {
        Ok(self.active()?.keys().cloned().collect())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OperationResult> {
        self.events.subscribe()
    }

    pub fn contains(&self, action: &str) -> BackendResult<bool> {
        Ok(self.active()?.values().any(|task| task.action == action))
    }

    // Callers hold the backend operation gate across staging and launch. The
    // start barrier guarantees registration and the started event precede work.
    pub fn launch<W, A>(
        self: &Arc<Self>,
        action: &'static str,
        message: &str,
        files: Vec<PathBuf>,
        work: impl FnOnce(Arc<OperationControl>) -> W + Send + 'static,
        after: impl FnOnce(OperationResult) -> A + Send + 'static,
    ) -> BackendResult<OperationResult>
    where
        W: Future<Output = BackendResult<OperationCompletion>> + Send + 'static,
        A: Future<Output = ()> + Send + 'static,
    {
        let mut active = self.active().inspect_err(|_| remove_files(&files))?;
        let mut operation = OperationResult::completed(action, message);
        operation.status = "started".into();
        let id = operation.id.clone();
        let control = Arc::new(OperationControl::default());
        let guard = control.clone();
        let operations = self.clone();
        let (start, ready) = oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Some(event) = operations.complete((id, action), ready, guard, work).await {
                after(event).await;
            }
        });
        active.insert(
            operation.id.clone(),
            OperationTask {
                action,
                control,
                handle,
                files,
            },
        );
        drop(active);
        let _ = self.events.send(operation.clone());
        start
            .send(())
            .map_err(|_| operation_error("Clipboard operation could not start"))?;
        Ok(operation)
    }

    // Claim under the same lock as cancellation: only one path may emit a
    // terminal event and remove staged files. Publish before waking waiters.
    async fn complete<W>(
        &self,
        (id, action): (String, &'static str),
        ready: oneshot::Receiver<()>,
        control: Arc<OperationControl>,
        work: impl FnOnce(Arc<OperationControl>) -> W,
    ) -> Option<OperationResult>
    where
        W: Future<Output = BackendResult<OperationCompletion>>,
    {
        let result = match ready.await {
            Ok(()) => work(control.clone()).await,
            Err(_) => Err(operation_error("Clipboard operation could not start")),
        };
        let claimed = self
            .active()
            .is_ok_and(|mut active| active.remove(&id).is_some());
        let event = claimed.then(|| {
            let event = match result {
                Ok(completion) => {
                    let mut event =
                        OperationResult::with_id(id, action, completion.status, completion.message);
                    event.warning = completion.warning;
                    event
                }
                Err(error) => OperationResult::with_id(id, action, "failed", &error.to_string()),
            };
            let _ = self.events.send(event.clone());
            event
        });
        control.finish();
        event
    }

    pub async fn cancel(&self, id: &str) -> BackendResult<bool> {
        let operation = {
            let mut active = self.active()?;
            if !active.get(id).is_some_and(|task| task.control.cancel()) {
                // Committing jobs retain their files and report their real outcome.
                return Ok(false);
            }
            active.remove(id)
        };
        let Some(mut task) = operation else {
            return Ok(false);
        };
        task.handle.abort();
        let _ = (&mut task.handle).await;
        let control = task.control.clone();
        let action = task.action;
        drop(task);
        let _ = self.events.send(OperationResult::with_id(
            id.to_owned(),
            action,
            "cancelled",
            "Image operation cancelled",
        ));
        control.finish();
        Ok(true)
    }

    pub async fn stop(&self) -> BackendResult<()> {
        let active: Vec<_> = self
            .active()?
            .iter()
            .map(|(id, task)| (id.clone(), task.control.clone()))
            .collect();
        for (id, control) in active {
            if !self.cancel(&id).await? {
                // The committing job needs the backend transaction lock; never
                // hold it while draining operations.
                control.wait().await;
            }
        }
        Ok(())
    }
}

pub(super) fn runtime_directory(child: &str) -> BackendResult<PathBuf> {
    let root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    private_directory(root.join(child))
}

pub(super) fn private_directory(path: PathBuf) -> BackendResult<PathBuf> {
    fs::create_dir_all(&path).map_err(operation_error)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(operation_error)?;
    Ok(path)
}

pub(super) fn private_file(path: &Path) -> BackendResult<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(operation_error)
}

pub(super) fn unique_path(directory: &Path, extension: &str) -> PathBuf {
    directory.join(format!("clipboard-{}.{}", uuid::Uuid::new_v4(), extension))
}

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

    fn cancel(&self) -> bool {
        self.phase
            .compare_exchange(EDITING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn finish(&self) {
        self.phase.store(FINISHED, Ordering::SeqCst);
        self.finished.notify_waiters();
    }

    async fn wait(&self) {
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
impl super::RingboardBackend {
    pub(crate) fn test_operation(
        &self,
        finish: oneshot::Receiver<bool>,
    ) -> BackendResult<OperationResult> {
        self.operations.launch(
            "annotate",
            "Started",
            vec![],
            move |_| async move {
                if !finish.await.map_err(operation_error)? {
                    return Err(operation_error("Injected failure"));
                }
                Ok(OperationCompletion {
                    status: "completed",
                    message: "Done",
                    warning: None,
                })
            },
            |_| async {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{OperationCompletion, Operations};
    use std::{fs, sync::Arc, time::Duration};

    #[tokio::test]
    async fn cancellation_removes_files_and_emits_one_terminal_event() {
        for start_first in [false, true] {
            let operations = Arc::new(Operations::default());
            let mut events = operations.subscribe();
            let directory = tempfile::tempdir().unwrap();
            let staged = directory.path().join("staged.png");
            fs::write(&staged, b"fixture").unwrap();
            let (started, running) = tokio::sync::oneshot::channel();
            let operation = operations
                .launch(
                    "annotate",
                    "Started",
                    vec![staged.clone()],
                    move |_| async move {
                        let _ = started.send(());
                        std::future::pending().await
                    },
                    |_| async {},
                )
                .unwrap();
            assert_eq!(events.try_recv().unwrap().status, "started");
            if start_first {
                running.await.unwrap();
            }
            assert!(operations.cancel(&operation.id).await.unwrap());
            assert!(!operations.cancel(&operation.id).await.unwrap());
            let event = events.try_recv().unwrap();
            assert_eq!(event.status, "cancelled");
            assert_eq!(event.id, operation.id);
            assert!(events.try_recv().is_err());
            assert!(!staged.exists());
            assert!(!operations.contains("annotate").unwrap());
        }
    }

    #[tokio::test]
    async fn cleanup_waits_for_a_blocking_commit_and_keeps_its_real_outcome() {
        let operations = Arc::new(Operations::default());
        let mut events = operations.subscribe();
        let directory = tempfile::tempdir().unwrap();
        let staged = directory.path().join("staged.png");
        fs::write(&staged, b"fixture").unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, running) = tokio::sync::oneshot::channel();
        let operation = operations
            .launch(
                "annotate",
                "Started",
                vec![staged.clone()],
                move |control| async move {
                    assert!(control.begin_commit());
                    tokio::task::spawn_blocking(move || {
                        let _ = entered.send(());
                        blocked.recv().unwrap();
                    })
                    .await
                    .unwrap();
                    Ok(OperationCompletion {
                        status: "completed",
                        message: "Committed",
                        warning: Some("Publication failed".into()),
                    })
                },
                |_| async {},
            )
            .unwrap();
        assert_eq!(events.try_recv().unwrap().status, "started");
        running.await.unwrap();
        assert!(!operations.cancel(&operation.id).await.unwrap());
        assert!(operations.active_ids().unwrap().contains(&operation.id));
        assert!(staged.exists());
        assert!(events.try_recv().is_err());
        let other = operations.clone();
        let cleanup = tokio::spawn(async move { other.stop().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!cleanup.is_finished());
        release.send(()).unwrap();
        cleanup.await.unwrap().unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.status, "completed");
        assert_eq!(event.warning.as_deref(), Some("Publication failed"));
        assert!(operations.active_ids().unwrap().is_empty());
        assert!(events.try_recv().is_err());
        assert!(!staged.exists());
    }
}
