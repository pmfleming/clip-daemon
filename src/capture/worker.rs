use std::{
    fs::File,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
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

pub(super) struct Control {
    pub gate: Admission,
    stop: AtomicBool,
    status: Mutex<Result<bool, String>>,
}

pub(super) struct Session {
    pub control: Arc<Control>,
    pub jobs: SyncSender<Job>,
    pub budget: Arc<Budget>,
    pub limit: u64,
    ready: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
}

impl Session {
    pub fn stopped(&self) -> bool {
        self.control.stop.load(Ordering::SeqCst) || self.control.gate.admit().is_none()
    }

    pub fn report(&self, result: Result<(), String>) {
        if let Ok(mut status) = self.control.status.lock() {
            *status = result.clone().map(|()| false);
        }
        if let Ok(mut ready) = self.ready.lock()
            && let Some(ready) = ready.take()
        {
            let _ = ready.send(result);
        }
    }

    fn run(
        self: Arc<Self>,
        sink: Arc<dyn CaptureSink>,
        incoming: Receiver<Job>,
        skip_initial: bool,
    ) {
        let control = self.control.clone();
        let ingest = sink.clone();
        let sink_thread = std::thread::Builder::new()
            .name("clip-ingest".into())
            .spawn(move || {
                while let Ok(job) = incoming.recv() {
                    if control
                        .gate
                        .submit(job.generation, || ingest.ingest(&job.mime, &job.file))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        let Ok(sink_thread) = sink_thread else {
            self.report(Err("Could not start capture ingest worker".into()));
            return;
        };
        self.reconnect(sink.as_ref(), skip_initial);
        // Control has no sender: dropping the session disconnects and drains the
        // queue even while Worker retains control for the final submission fence.
        drop(self);
        let _ = sink_thread.join();
    }

    fn reconnect(self: &Arc<Self>, sink: &dyn CaptureSink, mut skip_initial: bool) {
        let mut backoff = Duration::from_millis(250);
        while !self.control.stop.load(Ordering::SeqCst) {
            let result = sink
                .ready(self.limit)
                .and_then(|()| self.control.gate.resume())
                .and_then(|()| super::wayland::run(self.clone(), skip_initial));
            self.control.gate.close();
            if let Err(error) = self.control.gate.fence() {
                self.report(Err(error));
                break;
            }
            if result.is_err() {
                self.report(result);
            }
            skip_initial = true;
            let deadline = Instant::now() + backoff;
            while !self.control.stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }
}

struct Worker {
    thread: JoinHandle<()>,
    control: Arc<Control>,
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
        let control = Arc::new(Control {
            gate: Admission::default(),
            stop: AtomicBool::new(false),
            status: Mutex::new(Err("Capture is starting".into())),
        });
        let session = Arc::new(Session {
            control: control.clone(),
            budget: Arc::new(Budget::default()),
            ready: Mutex::new(Some(ready)),
            jobs,
            limit,
        });
        let thread = std::thread::Builder::new()
            .name("clip-capture".into())
            .spawn(move || session.run(sink, incoming, skip_initial))
            .map_err(|_| "Could not start capture worker")?;
        Ok((
            Self {
                thread,
                control,
                limit,
            },
            acknowledged,
        ))
    }

    fn state(&self) -> Result<bool, String> {
        if self.thread.is_finished() {
            return Err("Capture worker exited".into());
        }
        if self.control.gate.admit().is_none() {
            return Err("Capture is unavailable or transitioning".into());
        }
        self.control
            .status
            .lock()
            .map_err(|_| "Capture state is unavailable")?
            .clone()
    }

    fn close(&self) {
        self.control.stop.store(true, Ordering::SeqCst);
        self.control.gate.close();
    }

    fn finish(self) -> Result<(), String> {
        self.close();
        self.thread.join().map_err(|_| "Capture worker panicked")?;
        self.control.gate.fence()
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
    use super::{CaptureControl, CaptureSink, Control, Controller, Worker};
    use crate::capture::Admission;
    use std::{
        fs::File,
        sync::{Arc, Mutex, atomic::AtomicBool},
    };

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
    async fn unavailable_capture_can_pause_and_shutdown_but_never_restart_after_shutdown() {
        let controller = Controller::new(Arc::new(Unavailable));
        assert!(controller.is_paused().await.unwrap());
        assert!(controller.set_paused(false, 65536).await.is_err());
        assert!(controller.is_paused().await.is_err());
        controller.set_paused(true, 65536).await.unwrap();
        assert!(controller.is_paused().await.unwrap());
        controller.shutdown().await.unwrap();
        assert!(controller.set_paused(false, 65536).await.is_err());
        assert!(controller.is_paused().await.is_err());
    }

    #[tokio::test]
    async fn uncertain_shutdown_is_latched_across_repeated_privacy_requests() {
        let gate = Admission::default();
        gate.resume().unwrap();
        let generation = gate.admit().unwrap();
        assert!(
            gate.submit::<()>(generation, || Err("injected lost reply".into()))
                .is_err()
        );
        let controller = Controller::new(Arc::new(Unavailable));
        controller.runtime.lock().await.worker = Some(Worker {
            thread: std::thread::spawn(|| {}),
            control: Arc::new(Control {
                gate,
                stop: AtomicBool::new(false),
                status: Mutex::new(Ok(false)),
            }),
            limit: 65536,
        });
        for paused in [true, true, false] {
            assert!(controller.set_paused(paused, 65536).await.is_err());
            assert!(controller.is_paused().await.is_err());
        }
    }
}
