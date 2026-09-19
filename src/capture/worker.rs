use std::{
    fs::File,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use super::{
    Admission, CaptureControl, Generation,
    transfer::{Budget, Reservation},
};

/// Storage errors mean an uncertain outcome. Safe rejection returns success.
pub trait CaptureSink: Send + Sync + 'static {
    fn ready(&self, max_bytes: u64) -> Result<(), String>;
    fn ingest(&self, mime: &str, file: &File) -> Result<(), String>;
}

pub(super) struct Job {
    pub file: File,
    pub mime: String,
    pub generation: Generation,
    pub _reservation: Reservation,
}

pub(super) struct Session {
    pub gate: Arc<Admission>,
    pub jobs: SyncSender<Job>,
    pub budget: Arc<Budget>,
    pub limit: u64,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Result<bool, String>>>,
    ready: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
}

impl Session {
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.gate.admit().is_none()
    }

    pub fn running(&self) {
        self.set_status(Ok(false));
        self.ack(Ok(()));
    }

    fn set_status(&self, value: Result<bool, String>) {
        if let Ok(mut status) = self.status.lock() {
            *status = value;
        }
    }

    fn ack(&self, result: Result<(), String>) {
        if let Ok(mut ready) = self.ready.lock()
            && let Some(ready) = ready.take()
        {
            let _ = ready.send(result);
        }
    }
}

struct Worker {
    thread: JoinHandle<()>,
    gate: Arc<Admission>,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Result<bool, String>>>,
    limit: u64,
}

impl Worker {
    fn start(
        sink: Arc<dyn CaptureSink>,
        limit: u64,
        skip_initial: bool,
    ) -> Result<(Self, oneshot::Receiver<Result<(), String>>), String> {
        let (jobs, incoming) = mpsc::sync_channel::<Job>(1);
        let (ready, acknowledged) = oneshot::channel();
        let gate = Arc::new(Admission::default());
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Err("Capture is starting".into())));
        let session = Arc::new(Session {
            gate: gate.clone(),
            stop: stop.clone(),
            status: status.clone(),
            budget: Arc::new(Budget::default()),
            ready: Mutex::new(Some(ready)),
            jobs,
            limit,
        });
        let thread = std::thread::Builder::new()
            .name("clip-capture".into())
            .spawn(move || {
                let sink_gate = session.gate.clone();
                let ingest = sink.clone();
                let sink_thread = std::thread::Builder::new()
                    .name("clip-ingest".into())
                    .spawn(move || {
                        while let Ok(job) = incoming.recv() {
                            if sink_gate
                                .submit(job.generation, || ingest.ingest(&job.mime, &job.file))
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                let Ok(sink_thread) = sink_thread else {
                    let error = "Could not start capture ingest worker".to_owned();
                    session.set_status(Err(error.clone()));
                    session.ack(Err(error));
                    return;
                };
                let mut skip_initial = skip_initial;
                let mut backoff = Duration::from_millis(250);
                while !session.stop.load(Ordering::SeqCst) {
                    let result = sink
                        .ready(limit)
                        .and_then(|()| session.gate.resume())
                        .and_then(|()| super::wayland::run(session.clone(), skip_initial));
                    session.gate.close();
                    if let Err(error) = session.gate.fence() {
                        session.set_status(Err(error.clone()));
                        session.ack(Err(error));
                        break;
                    }
                    if let Err(error) = result {
                        session.set_status(Err(error.clone()));
                        session.ack(Err(error));
                    }
                    skip_initial = true;
                    let deadline = Instant::now() + backoff;
                    while !session.stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
                // Drop the last sender before joining, draining invalid generations.
                drop(session);
                let _ = sink_thread.join();
            })
            .map_err(|_| "Could not start capture worker")?;
        Ok((
            Self {
                thread,
                gate,
                stop,
                status,
                limit,
            },
            acknowledged,
        ))
    }

    fn state(&self) -> Result<bool, String> {
        if self.thread.is_finished() {
            return Err("Capture worker exited".into());
        }
        if self.gate.admit().is_none() {
            return Err("Capture is unavailable or transitioning".into());
        }
        self.status
            .lock()
            .map_err(|_| "Capture state is unavailable")?
            .clone()
    }

    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.gate.close();
    }

    fn finish(self) -> Result<(), String> {
        self.close();
        self.thread.join().map_err(|_| "Capture worker panicked")?;
        self.gate.fence()
    }
}

#[derive(Default)]
struct Runtime {
    worker: Option<Worker>,
    attached: bool,
    terminal_error: Option<String>,
}

impl Runtime {
    async fn stop(&mut self) -> Result<(), String> {
        if let Some(error) = &self.terminal_error {
            return Err(error.clone());
        }
        if let Some(worker) = self.worker.take() {
            worker.close();
            // A cancelled waiter leaves a sticky unverified state, not a false
            // successful pause while the detached fence is still pending.
            self.terminal_error = Some("Capture shutdown is not yet verified".into());
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || worker.finish()),
            )
            .await
            .map_err(|_| "Capture shutdown timed out".to_owned())
            .and_then(|result| result.map_err(|_| "Capture shutdown task failed".to_owned()))
            .and_then(|result| result);
            self.terminal_error = result.as_ref().err().cloned();
            result?;
        }
        Ok(())
    }
}

pub struct Controller {
    sink: Arc<dyn CaptureSink>,
    runtime: AsyncMutex<Runtime>,
}

impl Controller {
    pub fn new(sink: Arc<dyn CaptureSink>) -> Self {
        Self {
            sink,
            runtime: AsyncMutex::new(Runtime::default()),
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(worker) = &self.runtime.get_mut().worker {
            worker.close();
        }
    }
}

#[async_trait]
impl CaptureControl for Controller {
    async fn set_paused(&self, paused: bool, max_bytes: u64) -> Result<(), String> {
        let mut runtime = self.runtime.lock().await;
        if let Some(error) = &runtime.terminal_error {
            return Err(error.clone());
        }
        let limit = max_bytes.min(crate::backend::MAX_WAYLAND_SELECTION_BYTES);
        if !paused
            && let Some(worker) = &runtime.worker
            && worker.limit == limit
        {
            return worker.state().map(|_| ());
        }
        runtime.stop().await?;
        if paused {
            runtime.attached = true; // Explicit pause, including private startup: skip bootstrap on resume.
            return Ok(());
        }
        if limit == 0 {
            return Err("Capture byte limit must be positive".into());
        }
        let (worker, ready) = Worker::start(self.sink.clone(), limit, runtime.attached)?;
        runtime.attached = true;
        runtime.worker = Some(worker);
        tokio::time::timeout(Duration::from_secs(6), ready)
            .await
            .map_err(|_| "Capture startup acknowledgement timed out")?
            .map_err(|_| "Capture worker exited before startup")?
    }

    async fn shutdown(&self) -> Result<(), String> {
        let mut runtime = self.runtime.lock().await;
        let result = runtime.stop().await;
        runtime.terminal_error = Some("Capture owner has shut down".into());
        result
    }

    async fn is_paused(&self) -> Result<bool, String> {
        let runtime = self.runtime.lock().await;
        if let Some(error) = &runtime.terminal_error {
            return Err(error.clone());
        }
        match &runtime.worker {
            None => Ok(true),
            Some(worker) => worker.state(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Unavailable;
    impl CaptureSink for Unavailable {
        fn ready(&self, _: u64) -> Result<(), String> {
            Err("injected unavailable engine".into())
        }
        fn ingest(&self, _: &str, _: &File) -> Result<(), String> {
            panic!("admission should stay closed")
        }
    }

    #[tokio::test]
    async fn missing_engine_never_claims_running_and_pause_joins_workers() {
        let controller = Controller::new(Arc::new(Unavailable));
        assert!(controller.is_paused().await.unwrap());
        assert!(controller.set_paused(false, 65536).await.is_err());
        assert!(controller.is_paused().await.is_err());
        controller.set_paused(true, 65536).await.unwrap();
        assert!(controller.is_paused().await.unwrap());
    }

    #[tokio::test]
    async fn shutdown_cannot_be_undone_by_a_late_resume() {
        let controller = Controller::new(Arc::new(Unavailable));
        controller.shutdown().await.unwrap();
        assert!(controller.set_paused(false, 65536).await.is_err());
        assert!(controller.is_paused().await.is_err());
    }

    #[tokio::test]
    async fn uncertain_shutdown_is_latched_across_repeated_privacy_requests() {
        let gate = Arc::new(Admission::default());
        gate.resume().unwrap();
        let generation = gate.admit().unwrap();
        assert!(
            gate.submit::<()>(generation, || Err("injected lost reply".into()))
                .is_err()
        );
        let controller = Controller::new(Arc::new(Unavailable));
        controller.runtime.lock().await.worker = Some(Worker {
            thread: std::thread::spawn(|| {}),
            gate,
            stop: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(Ok(false))),
            limit: 65536,
        });
        for paused in [true, true, false] {
            assert!(controller.set_paused(paused, 65536).await.is_err());
            assert!(controller.is_paused().await.is_err());
        }
    }
}
