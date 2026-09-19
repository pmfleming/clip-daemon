//! Explicit user captures: no shell, clipboard subprocess, or history-entry race.
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

use tokio::{io::AsyncReadExt, process::Command, sync::oneshot};

use super::mutation::{
    operation_error, private_file, runtime_directory, unique_path, valid_edited_image,
};
use super::{OperationControl, OperationTask, RingboardBackend, run_blocking};
use crate::{
    backend::{BackendResult, InteractiveScreenshot, ScreenshotMode},
    model::OperationResult,
};

pub(super) fn launch(
    backend: RingboardBackend,
    request: InteractiveScreenshot,
    max_bytes: u64,
) -> BackendResult<OperationResult> {
    let mut active = backend.operations.lock().map_err(|_| super::lock_error())?;
    if active.values().any(|job| job.action == "screenshot") {
        return Err(operation_error("A screenshot is already in progress"));
    }
    let directory = runtime_directory("clip-daemon/screenshots")?;
    let input = unique_path(&directory, "png");
    let output = unique_path(&directory, "png");
    drop(private_file(&input)?);
    let mut operation = OperationResult::completed("screenshot", "Screenshot started");
    operation.status = "started".into();
    let id = operation.id.clone();
    let control = Arc::new(OperationControl::default());
    let (start, ready) = oneshot::channel();
    let worker = backend.clone();
    let guard = control.clone();
    let files = vec![input.clone(), output.clone()];
    let task_id = id.clone();
    let handle = tokio::spawn(async move {
        let result = async {
            ready.await.map_err(operation_error)?;
            capture(&worker, request, &input, &output, max_bytes, &guard).await
        }
        .await;
        let claimed = worker
            .operations
            .lock()
            .is_ok_and(|mut jobs| jobs.remove(&task_id).is_some());
        if claimed {
            let (status, message) = match result {
                Ok(true) => ("completed", "Screenshot copied".to_owned()),
                Ok(false) => ("cancelled", "Screenshot cancelled".to_owned()),
                Err(error) => ("failed", error.to_string()),
            };
            let _ = worker.operation_events.send(OperationResult::with_id(
                task_id,
                "screenshot",
                status,
                &message,
            ));
            guard.finish();
            if status != "cancelled" {
                let mut notification = Command::new("notify-send");
                notification
                    .args(["-a", "clip-daemon", "Screenshot", &message])
                    .kill_on_drop(true);
                let _ = tokio::time::timeout(Duration::from_secs(3), notification.status()).await;
            }
        } else {
            guard.finish();
        }
    });
    active.insert(
        id,
        OperationTask {
            action: "screenshot",
            control,
            handle,
            files,
        },
    );
    drop(active);
    let _ = backend.operation_events.send(operation.clone());
    let _ = start.send(());
    Ok(operation)
}

async fn capture(
    backend: &RingboardBackend,
    request: InteractiveScreenshot,
    input: &Path,
    output: &Path,
    max_bytes: u64,
    control: &OperationControl,
) -> BackendResult<bool> {
    let mut command = Command::new("grim");
    if matches!(request.mode, ScreenshotMode::Region) {
        let Some(geometry) = select_region("slurp").await? else {
            return Ok(false);
        };
        command.args(["-g", &geometry]);
    }
    command.arg(input).kill_on_drop(true).stdin(Stdio::null());
    let status = tokio::time::timeout(Duration::from_secs(15), command.status())
        .await
        .map_err(operation_error)?
        .map_err(operation_error)?;
    if !status.success() {
        return Err(operation_error("Screenshot capture failed"));
    }
    let chosen = if request.annotate {
        let path = input.to_owned();
        if !run_blocking(move || Ok(valid_edited_image(&path, max_bytes))).await? {
            return Err(operation_error("Screenshot is not a valid bounded PNG"));
        }
        tokio::time::timeout(Duration::from_secs(1800), backend.editor.run(input, output))
            .await
            .map_err(operation_error)?
            .map_err(operation_error)?;
        if !output.exists() {
            return Ok(false);
        }
        output.to_owned()
    } else {
        input.to_owned()
    };
    if !control.begin_commit() {
        return Ok(false);
    }
    let backend = backend.clone();
    run_blocking(move || {
        let _transaction = backend
            .transaction
            .lock()
            .map_err(|_| super::lock_error())?;
        if !valid_edited_image(&chosen, max_bytes) {
            return Err(operation_error("Screenshot is not a valid bounded PNG"));
        }
        backend
            .selection
            .publish_file("image/png", &chosen, max_bytes)?;
        backend.selection_changed()?;
        Ok(true)
    })
    .await
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
    let (x, y) = position.split_once(',').ok_or_else(invalid)?;
    let (width, height) = size.split_once('x').ok_or_else(invalid)?;
    let (x, y) = (
        x.parse::<i32>().map_err(|_| invalid())?,
        y.parse::<i32>().map_err(|_| invalid())?,
    );
    let (width, height) = (
        width.parse::<u32>().map_err(|_| invalid())?,
        height.parse::<u32>().map_err(|_| invalid())?,
    );
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > 32 * 1024 * 1024
    {
        return Err(invalid());
    }
    Ok(format!("{x},{y} {width}x{height}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn regions_are_bounded_and_canonical() {
        assert_eq!(
            parse_geometry("-1920,0 1920x1080\n").unwrap(),
            "-1920,0 1920x1080"
        );
        for value in [
            "",
            "0,0 0x1",
            "0,0 16384x16384",
            "0,0 1x1; rm -rf /",
            "2147483648,0 1x1",
            "0,0 -1x1",
        ] {
            assert!(parse_geometry(value).is_err(), "{value}");
        }
    }

    #[tokio::test]
    async fn selector_cancellation_never_becomes_full_screen_capture() {
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
        std::fs::write(&script, "#!/bin/sh\nprintf 'garbage'\n").unwrap();
        assert!(select_region(script.to_str().unwrap()).await.is_err());
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '{}'\n", "x".repeat(129)),
        )
        .unwrap();
        assert!(select_region(script.to_str().unwrap()).await.is_err());
    }

    #[test]
    fn requests_reject_unknown_modes_and_fields() {
        assert!(
            serde_json::from_str::<InteractiveScreenshot>(r#"{"mode":"region","command":"evil"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<InteractiveScreenshot>(r#"{"mode":"window"}"#).is_err());
    }
}
