use std::path::PathBuf;

use tokio::process::Command;
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult},
    model::{FilePreview, OperationResult},
};

pub async fn open_url(value: &str) -> BackendResult<OperationResult> {
    let url = Url::parse(value.trim()).map_err(|_| invalid("URL is malformed"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("Only HTTP and HTTPS URLs can be opened directly"));
    }
    spawn("xdg-open", &[url.as_str()])?;
    Ok(OperationResult::completed("open-url", "URL opened"))
}

pub async fn open_file(file: &FilePreview, reveal: bool) -> BackendResult<OperationResult> {
    let path = local_path(file)?;
    if !path.exists() {
        return Err(BackendError::not_found("Clipboard file no longer exists"));
    }
    let path = path.to_string_lossy();
    if reveal {
        spawn("dolphin", &["--select", path.as_ref()])?;
        Ok(OperationResult::completed("reveal-file", "File revealed"))
    } else {
        spawn("xdg-open", &[path.as_ref()])?;
        Ok(OperationResult::completed("open-file", "File opened"))
    }
}

fn local_path(file: &FilePreview) -> BackendResult<PathBuf> {
    Url::parse(&file.uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .ok_or_else(|| invalid("Clipboard entry is not a local file"))
}

fn spawn(program: &str, arguments: &[&str]) -> BackendResult<()> {
    Command::new(program)
        .args(arguments)
        .spawn()
        .map(|_| ())
        .map_err(|_| {
            BackendError::new(
                BackendErrorKind::OperationFailed,
                "Application launch failed",
            )
        })
}

fn invalid(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use crate::model::FilePreview;

    use super::local_path;

    #[test]
    fn only_local_file_uris_are_resolved() {
        let local = FilePreview {
            display_name: "a".into(),
            uri: "file:///tmp/a".into(),
            exists: false,
            operation: "copy".into(),
        };
        let remote = FilePreview {
            uri: "https://example.test/a".into(),
            ..local.clone()
        };
        assert_eq!(local_path(&local).unwrap().to_string_lossy(), "/tmp/a");
        assert!(local_path(&remote).is_err());
    }
}
