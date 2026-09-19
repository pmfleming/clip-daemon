//! Disposable-test driver for the collector before production cutover.
//! Run only with isolated HOME/XDG/RINGBOARD_SOCK/Wayland environment.
use clip_daemon::{
    capture::{CaptureControl, Controller},
    ringboard::{RingboardBackend, capture::RingboardCapture},
};
use std::{
    io::{self, BufRead, Write},
    sync::Arc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let limit: u64 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "65536".into())
        .parse()?;
    let controller = Controller::new(Arc::new(RingboardCapture::new(RingboardBackend::default())));
    for command in io::stdin().lock().lines() {
        let result = match command?.as_str() {
            "resume" => controller.set_paused(false, limit).await.map(|()| false),
            "pause" => controller.set_paused(true, limit).await.map(|()| true),
            "status" => controller.is_paused().await,
            "quit" => {
                controller
                    .set_paused(true, limit)
                    .await
                    .map_err(anyhow::Error::msg)?;
                break;
            }
            _ => Err("Unknown fixture command".into()),
        };
        println!("{}", serde_json::to_string(&result)?);
        io::stdout().flush()?;
    }
    controller
        .set_paused(true, limit)
        .await
        .map_err(anyhow::Error::msg)
}
