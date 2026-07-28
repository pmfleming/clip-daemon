use std::{fs::File, io::Read, path::Path, sync::Arc};

use wl_clipboard_rs::copy::{ClipboardType, MimeSource, MimeType, Options, Seat, Source};

use crate::backend::{BackendError, BackendErrorKind, BackendResult};

pub use crate::backend::MAX_WAYLAND_SELECTION_BYTES;

pub trait SelectionPublisher: Send + Sync {
    fn publish(&self, mime: &str, bytes: Vec<u8>) -> BackendResult<()>;

    fn publish_file_link(&self, uri: Vec<u8>) -> BackendResult<()> {
        self.publish("text/uri-list", uri)
    }

    fn publish_files(&self, gnome_payload: Vec<u8>, uri_list: Vec<u8>) -> BackendResult<()> {
        let _ = uri_list;
        self.publish("x-special/gnome-copied-files", gnome_payload)
    }
}

#[derive(Default)]
pub struct WaylandSelectionPublisher;

impl SelectionPublisher for WaylandSelectionPublisher {
    fn publish(&self, mime: &str, bytes: Vec<u8>) -> BackendResult<()> {
        let mut options = Options::new();
        options
            .clipboard(ClipboardType::Regular)
            .seat(Seat::All)
            .omit_additional_text_mime_types(true);
        options
            .copy(
                Source::Bytes(bytes.into_boxed_slice()),
                MimeType::Specific(mime.to_owned()),
            )
            .map_err(|error| selection_error(error.to_string()))
    }

    fn publish_file_link(&self, uri: Vec<u8>) -> BackendResult<()> {
        let text = uri.strip_suffix(b"\r\n").unwrap_or(&uri).to_vec();
        let mut options = Options::new();
        options.clipboard(ClipboardType::Regular).seat(Seat::All);
        options
            .copy_multi(vec![
                MimeSource {
                    source: Source::Bytes(uri.into_boxed_slice()),
                    mime_type: MimeType::Specific("text/uri-list".into()),
                },
                MimeSource {
                    source: Source::Bytes(text.into_boxed_slice()),
                    mime_type: MimeType::Text,
                },
            ])
            .map_err(|error| selection_error(error.to_string()))
    }

    fn publish_files(&self, gnome_payload: Vec<u8>, uri_list: Vec<u8>) -> BackendResult<()> {
        let mut options = Options::new();
        options
            .clipboard(ClipboardType::Regular)
            .seat(Seat::All)
            .omit_additional_text_mime_types(true);
        options
            .copy_multi(vec![
                MimeSource {
                    source: Source::Bytes(gnome_payload.into_boxed_slice()),
                    mime_type: MimeType::Specific("x-special/gnome-copied-files".into()),
                },
                MimeSource {
                    source: Source::Bytes(uri_list.into_boxed_slice()),
                    mime_type: MimeType::Specific("text/uri-list".into()),
                },
            ])
            .map_err(|error| selection_error(error.to_string()))
    }
}

#[derive(Clone)]
pub struct SelectionService {
    publisher: Arc<dyn SelectionPublisher>,
}

impl Default for SelectionService {
    fn default() -> Self {
        Self {
            publisher: Arc::new(WaylandSelectionPublisher),
        }
    }
}

impl SelectionService {
    #[cfg(test)]
    pub fn with_publisher(publisher: Arc<dyn SelectionPublisher>) -> Self {
        Self { publisher }
    }

    pub fn publish(&self, mime: &str, bytes: Vec<u8>, configured_limit: u64) -> BackendResult<()> {
        validate_mime(mime)?;
        validate_size(bytes.len() as u64, configured_limit)?;
        self.publisher.publish(mime, bytes)
    }

    pub fn publish_file_link(&self, uri: Vec<u8>, configured_limit: u64) -> BackendResult<()> {
        validate_size(uri.len() as u64, configured_limit)?;
        self.publisher.publish_file_link(uri)
    }

    pub fn publish_files(
        &self,
        operation: &str,
        uri_list: Vec<u8>,
        configured_limit: u64,
    ) -> BackendResult<()> {
        if !matches!(operation, "copy" | "cut") {
            return Err(BackendError::new(
                BackendErrorKind::InvalidData,
                "File selection operation must be copy or cut",
            ));
        }
        let mut gnome_payload = Vec::with_capacity(operation.len() + 1 + uri_list.len());
        gnome_payload.extend_from_slice(operation.as_bytes());
        gnome_payload.push(b'\n');
        gnome_payload.extend_from_slice(&uri_list);
        validate_size(uri_list.len() as u64, configured_limit)?;
        validate_size(gnome_payload.len() as u64, configured_limit)?;
        self.publisher.publish_files(gnome_payload, uri_list)
    }

    pub fn publish_file(
        &self,
        mime: &str,
        path: &Path,
        configured_limit: u64,
    ) -> BackendResult<()> {
        validate_mime(mime)?;
        let file = File::open(path).map_err(selection_io_error)?;
        let size = file.metadata().map_err(selection_io_error)?.len();
        validate_size(size, configured_limit)?;
        let limit = effective_limit(configured_limit);
        let mut bytes = Vec::with_capacity(size.min(limit) as usize);
        file.take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(selection_io_error)?;
        validate_size(bytes.len() as u64, configured_limit)?;
        self.publisher.publish(mime, bytes)
    }
}

pub fn effective_limit(configured_limit: u64) -> u64 {
    configured_limit.min(MAX_WAYLAND_SELECTION_BYTES)
}

fn validate_size(size: u64, configured_limit: u64) -> BackendResult<()> {
    let limit = effective_limit(configured_limit);
    if size > limit {
        return Err(BackendError::new(
            BackendErrorKind::InvalidData,
            format!(
                "Clipboard entry is {size} bytes; Wayland publishing is limited to {limit} bytes"
            ),
        ));
    }
    Ok(())
}

fn validate_mime(mime: &str) -> BackendResult<()> {
    (valid_mime_ascii(mime) && valid_mime_essence(mime))
        .then_some(())
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorKind::InvalidData,
                "Clipboard MIME type is invalid for Wayland publishing",
            )
        })
}

fn valid_mime_ascii(mime: &str) -> bool {
    !mime.is_empty()
        && mime.len() <= 255
        && mime
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
}

fn valid_mime_essence(mime: &str) -> bool {
    let mut parts = mime.split(';').next().unwrap_or_default().trim().split('/');
    parts.next().is_some_and(valid_mime_token)
        && parts.next().is_some_and(valid_mime_token)
        && parts.next().is_none()
}

fn valid_mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
}

fn selection_io_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, error.to_string())
}

fn selection_error(message: String) -> BackendError {
    BackendError::new(
        BackendErrorKind::OperationFailed,
        format!("Could not own the Wayland clipboard selection: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{
        MAX_WAYLAND_SELECTION_BYTES, SelectionPublisher, SelectionService, effective_limit,
    };
    use crate::backend::BackendResult;

    #[derive(Default)]
    struct RecordingPublisher {
        values: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl SelectionPublisher for RecordingPublisher {
        fn publish(&self, mime: &str, bytes: Vec<u8>) -> BackendResult<()> {
            self.values.lock().unwrap().push((mime.to_owned(), bytes));
            Ok(())
        }

        fn publish_file_link(&self, uri: Vec<u8>) -> BackendResult<()> {
            let text = uri.strip_suffix(b"\r\n").unwrap_or(&uri).to_vec();
            let mut values = self.values.lock().unwrap();
            values.push(("text/uri-list".into(), uri));
            values.push(("text/plain".into(), text));
            Ok(())
        }

        fn publish_files(&self, gnome_payload: Vec<u8>, uri_list: Vec<u8>) -> BackendResult<()> {
            let mut values = self.values.lock().unwrap();
            values.push(("x-special/gnome-copied-files".into(), gnome_payload));
            values.push(("text/uri-list".into(), uri_list));
            Ok(())
        }
    }

    #[test]
    fn exact_mime_and_bytes_reach_the_publisher() {
        let publisher = Arc::new(RecordingPublisher::default());
        let service = SelectionService::with_publisher(publisher.clone());
        service.publish("image/png", vec![1, 2, 3], 1024).unwrap();
        service
            .publish_file_link(b"file:///tmp/image.png\r\n".to_vec(), 1024)
            .unwrap();
        service
            .publish_files(
                "cut",
                b"file:///tmp/one.txt\r\nfile:///tmp/two.txt\r\n".to_vec(),
                1024,
            )
            .unwrap();
        assert_eq!(
            publisher.values.lock().unwrap().as_slice(),
            &[
                ("image/png".into(), vec![1, 2, 3]),
                (
                    "text/uri-list".into(),
                    b"file:///tmp/image.png\r\n".to_vec()
                ),
                ("text/plain".into(), b"file:///tmp/image.png".to_vec()),
                (
                    "x-special/gnome-copied-files".into(),
                    b"cut\nfile:///tmp/one.txt\r\nfile:///tmp/two.txt\r\n".to_vec()
                ),
                (
                    "text/uri-list".into(),
                    b"file:///tmp/one.txt\r\nfile:///tmp/two.txt\r\n".to_vec()
                )
            ]
        );
    }

    #[test]
    fn mime_and_size_policy_rejects_unsafe_offers() {
        let service = SelectionService::with_publisher(Arc::new(RecordingPublisher::default()));
        assert!(service.publish("not-a-mime", vec![], 1024).is_err());
        assert!(
            service
                .publish("text/plain\nimage/png", vec![], 1024)
                .is_err()
        );
        assert!(
            service
                .publish("text/uri-list", vec![0; 1025], 1024)
                .is_err()
        );
        assert_eq!(effective_limit(u64::MAX), MAX_WAYLAND_SELECTION_BYTES);
    }
}
