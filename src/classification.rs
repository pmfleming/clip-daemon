use crate::model::EntryKind;

pub const INSPECTION_LIMIT: usize = 64 * 1024;
const PREVIEW_LIMIT: usize = 256;

pub fn classify(mime: &str, bytes: &[u8]) -> EntryKind {
    let normalized = mime.split(';').next().unwrap_or(mime).trim();
    authoritative_kind(normalized)
        .unwrap_or_else(|| semantic_text_kind(&bytes[..bytes.len().min(INSPECTION_LIMIT)]))
}

pub fn bounded_preview(bytes: &[u8], max_bytes: usize) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(max_bytes)]);
    let normalized = text
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_utf8(normalized, PREVIEW_LIMIT)
}

fn authoritative_kind(mime: &str) -> Option<EntryKind> {
    match mime.to_ascii_lowercase().as_str() {
        "" | "text/plain" => None,
        "text/uri-list" | "x-special/gnome-copied-files" => Some(EntryKind::Files),
        "text/html" | "application/xhtml+xml" => Some(EntryKind::Html),
        "application/json" | "application/ld+json" => Some(EntryKind::Json),
        value if value.starts_with("image/") => Some(EntryKind::Image),
        value if value.starts_with("text/") => None,
        _ => Some(EntryKind::Binary),
    }
}

fn semantic_text_kind(bytes: &[u8]) -> EntryKind {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return EntryKind::Binary;
    };
    let value = text.trim();
    [
        (looks_like_url(value), EntryKind::Link),
        (looks_like_json(value), EntryKind::Json),
        (looks_like_color(value), EntryKind::Color),
    ]
    .into_iter()
    .find_map(|(matches, kind)| matches.then_some(kind))
    .unwrap_or(EntryKind::Text)
}

fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let boundary = (0..=limit)
        .rev()
        .find(|index| value.is_char_boundary(*index))
        .unwrap_or_default();
    value.truncate(boundary);
    value
}

fn looks_like_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && !rest.is_empty()
        && !value.chars().any(char::is_whitespace)
}

fn looks_like_json(value: &str) -> bool {
    matches!(value.as_bytes(), [b'{', .., b'}'] | [b'[', .., b']'])
        && serde_json::from_str::<serde_json::Value>(value).is_ok()
}

fn looks_like_color(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use super::{bounded_preview, classify};
    use crate::model::EntryKind;

    #[test]
    fn authoritative_mimes_win_over_text_sniffing() {
        assert_kind("image/png", b"https://example.test", EntryKind::Image);
        assert_kind("text/uri-list", b"file:///tmp/a", EntryKind::Files);
        assert_kind("application/octet-stream", b"hello", EntryKind::Binary);
    }

    #[test]
    fn bounded_text_inspection_refines_semantic_kinds() {
        assert_kind("text/plain", b"https://example.test/a", EntryKind::Link);
        assert_kind("text/plain", br#"{"ok":true}"#, EntryKind::Json);
        assert_kind("text/plain", b"#1a2b3c", EntryKind::Color);
        assert_kind("", b"ordinary text", EntryKind::Text);
        assert_kind("", &[0xff, 0xfe], EntryKind::Binary);
    }

    fn assert_kind(mime: &str, bytes: &[u8], expected: EntryKind) {
        assert_eq!(classify(mime, bytes), expected);
    }

    #[test]
    fn previews_are_single_line_and_bounded() {
        assert_eq!(bounded_preview(b" first\n\tsecond ", 1024), "first second");
        assert!(bounded_preview(&vec![b'a'; 1024], 1024).len() <= 256);
    }
}
