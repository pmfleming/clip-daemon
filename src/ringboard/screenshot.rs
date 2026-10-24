//! Explicit user captures: no shell, clipboard subprocess, or history-entry race.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{io::AsyncReadExt, process::Command};

use super::content::{MAX_THUMBNAIL_BYTES, valid_edited_image};
use super::operation::{
    OperationCompletion, OperationControl, private_file, runtime_directory, unique_path,
};
use super::{RingboardBackend, run_blocking};
use crate::{
    backend::{
        BackendResult, InteractiveScreenshot, ScreenshotMode, ScreenshotRegion, operation_error,
    },
    model::OperationResult,
};

/// Direct captures have no Operations entry to own their staging file.
struct StagedCapture(PathBuf);
impl Drop for StagedCapture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl RingboardBackend {
    pub(super) async fn capture_region(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        if !ScreenshotRegion::valid_dimensions(region.width, region.height) {
            return Err(operation_error("Invalid screenshot region"));
        }
        let path = unique_path(&runtime_directory("clip-daemon/screenshots")?, "png");
        drop(private_file(&path)?);
        let staged = StagedCapture(path);
        let mut command = Command::new("grim");
        command
            .args([
                "-g",
                &format!(
                    "{},{} {}x{}",
                    region.x, region.y, region.width, region.height
                ),
            ])
            .arg(&staged.0);
        capture_command(&mut command, Duration::from_secs(15)).await?;
        let backend = self.clone();
        run_blocking(move || {
            let _transaction = backend
                .transaction
                .lock()
                .map_err(|_| super::lock_error())?;
            publish_capture(&backend, &staged.0, max_bytes)?;
            Ok(OperationResult::completed(
                "screenshot",
                "Screenshot published to the Wayland clipboard",
            ))
        })
        .await
    }
}

fn publish_capture(backend: &RingboardBackend, path: &Path, max_bytes: u64) -> BackendResult<()> {
    if !valid_edited_image(path, max_bytes) {
        return Err(operation_error("Screenshot is not a valid bounded PNG"));
    }
    backend
        .selection
        .publish_file("image/png", path, max_bytes.min(MAX_THUMBNAIL_BYTES))?;
    backend.selection_changed()
}

async fn capture_command(command: &mut Command, timeout: Duration) -> BackendResult<()> {
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|_| operation_error("Could not start screenshot capture"))?;
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .map_err(|_| operation_error("Screenshot capture timed out"))
        .and_then(|status| status.map_err(operation_error));
    if status.is_err() {
        // Explicitly reap on timeout/I/O failure; kill_on_drop also handles a
        // cancelled caller while waiting. Neither path leaves grim running.
        let _ = child.kill().await;
    }
    status?
        .success()
        .then_some(())
        .ok_or_else(|| operation_error("Screenshot capture failed"))
}

pub(super) fn launch(
    backend: &RingboardBackend,
    request: InteractiveScreenshot,
    max_bytes: u64,
) -> BackendResult<OperationResult> {
    if backend.operations.contains("screenshot")? {
        return Err(operation_error("A screenshot is already in progress"));
    }
    let directory = runtime_directory("clip-daemon/screenshots")?;
    let input = unique_path(&directory, "png");
    let output = unique_path(&directory, "png");
    drop(private_file(&input)?);
    let worker = backend.clone();
    backend.operations.launch(
        "screenshot",
        "Screenshot started",
        vec![input.clone(), output.clone()],
        move |control| async move {
            capture(&worker, request, &input, &output, max_bytes, &control).await
        },
        notify,
    )
}

async fn notify(event: OperationResult) {
    if event.status == "cancelled" {
        return;
    }
    let mut notification = Command::new("notify-send");
    notification
        .args(["-a", "clip-daemon", "Screenshot", &event.message])
        .kill_on_drop(true);
    let _ = tokio::time::timeout(Duration::from_secs(3), notification.status()).await;
}

async fn capture(
    backend: &RingboardBackend,
    request: InteractiveScreenshot,
    input: &Path,
    output: &Path,
    max_bytes: u64,
    control: &OperationControl,
) -> BackendResult<OperationCompletion> {
    let cancelled = OperationCompletion {
        status: "cancelled",
        message: "Screenshot cancelled",
        warning: None,
    };
    if !grab(request.mode, input).await? {
        return Ok(cancelled);
    }
    if request.annotate && !edit_capture(backend, input, output, max_bytes).await? {
        return Ok(cancelled);
    }
    let chosen = if request.annotate { output } else { input }.to_owned();
    if !control.begin_commit() {
        return Ok(cancelled);
    }
    let backend = backend.clone();
    run_blocking(move || {
        let _transaction = backend
            .transaction
            .lock()
            .map_err(|_| super::lock_error())?;
        publish_capture(&backend, &chosen, max_bytes)?;
        Ok(OperationCompletion {
            status: "completed",
            message: "Screenshot copied",
            warning: None,
        })
    })
    .await
}

async fn grab(mode: ScreenshotMode, input: &Path) -> BackendResult<bool> {
    let mut command = Command::new("grim");
    if matches!(mode, ScreenshotMode::Region) {
        let Some(geometry) = select_region("slurp").await? else {
            return Ok(false);
        };
        command.args(["-g", &geometry]);
    }
    command.arg(input);
    capture_command(&mut command, Duration::from_secs(15)).await?;
    Ok(true)
}

async fn edit_capture(
    backend: &RingboardBackend,
    input: &Path,
    output: &Path,
    max_bytes: u64,
) -> BackendResult<bool> {
    let path = input.to_owned();
    if !run_blocking(move || Ok(valid_edited_image(&path, max_bytes))).await? {
        return Err(operation_error("Screenshot is not a valid bounded PNG"));
    }
    tokio::time::timeout(
        Duration::from_secs(1800),
        backend.operations.edit(input, output),
    )
    .await
    .map_err(operation_error)??;
    Ok(output.exists())
}

async fn select_region(program: &str) -> BackendResult<Option<String>> {
    let mut child = Command::new(program)
        .args(["-f", "%x,%y %wx%h"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(operation_error)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| operation_error("Missing selector output"))?;
    tokio::time::timeout(Duration::from_secs(300), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(129)
            .read_to_end(&mut bytes)
            .await
            .map_err(operation_error)?;
        if bytes.len() > 128 {
            return Err(operation_error("Invalid screenshot region"));
        }
        let status = child.wait().await.map_err(operation_error)?;
        if status.code() == Some(1) {
            return Ok(None);
        }
        if !status.success() {
            return Err(operation_error("Region selector failed"));
        }
        let value = std::str::from_utf8(&bytes).map_err(operation_error)?;
        parse_geometry(value).map(Some)
    })
    .await
    .map_err(operation_error)?
}

fn parse_geometry(value: &str) -> BackendResult<String> {
    let invalid = || operation_error("Invalid screenshot region");
    let (position, size) = value.trim().split_once(' ').ok_or_else(invalid)?;
    let (x, y) = parse_pair::<i32>(position, ',')?;
    let (width, height) = parse_pair::<u32>(size, 'x')?;
    if !ScreenshotRegion::valid_dimensions(width, height) {
        return Err(invalid());
    }
    Ok(format!("{x},{y} {width}x{height}"))
}

fn parse_pair<T: std::str::FromStr>(value: &str, separator: char) -> BackendResult<(T, T)> {
    let invalid = || operation_error("Invalid screenshot region");
    let (a, b) = value.split_once(separator).ok_or_else(invalid)?;
    Ok((
        a.parse().map_err(|_| invalid())?,
        b.parse().map_err(|_| invalid())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{StagedCapture, capture_command, select_region};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn screenshot_commands_are_bounded_and_staging_is_scoped() {
        use std::time::Duration;
        use tokio::process::Command;
        for (program, arguments, timeout, success) in [
            ("true", vec![], Duration::from_secs(2), true),
            ("false", vec![], Duration::from_secs(2), false),
            (
                "/no-such-clip-capture-command",
                vec![],
                Duration::from_secs(2),
                false,
            ),
            ("sleep", vec!["30"], Duration::from_millis(20), false),
        ] {
            let mut command = Command::new(program);
            command.args(arguments);
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                capture_command(&mut command, timeout),
            )
            .await
            .unwrap();
            assert_eq!(result.is_ok(), success, "{program}");
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.png");
        let pid_file = directory.path().join("pid");
        let task = tokio::spawn({
            let path = path.clone();
            let pid_file = pid_file.clone();
            async move {
                std::fs::write(&path, b"staged").unwrap();
                let _staged = StagedCapture(path);
                let mut command = Command::new("sh");
                command
                    .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "capture-test"])
                    .arg(pid_file);
                capture_command(&mut command, Duration::from_secs(30)).await
            }
        });
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|value| value.trim().parse().ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(path.exists());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!path.exists());
        tokio::time::timeout(Duration::from_secs(3), async {
            while rustix::process::test_kill_process(pid).is_ok() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("cancelled child must be killed and reaped");
    }

    #[tokio::test]
    async fn selector_accepts_only_bounded_regions_and_preserves_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("selector");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            select_region(script.to_str().unwrap())
                .await
                .unwrap()
                .is_none()
        );
        for value in [
            "".into(),
            "garbage".into(),
            "0,0 0x1".into(),
            "0,0 16384x16384".into(),
            "0,0 1x1; rm -rf /".into(),
            "2147483648,0 1x1".into(),
            "0,0 -1x1".into(),
            "x".repeat(129),
        ] {
            std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' '{value}'\n")).unwrap();
            assert!(
                select_region(script.to_str().unwrap()).await.is_err(),
                "{value}"
            );
        }
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '-1920,0 1920x1080'\n").unwrap();
        assert_eq!(
            select_region(script.to_str().unwrap())
                .await
                .unwrap()
                .as_deref(),
            Some("-1920,0 1920x1080")
        );
    }
}
