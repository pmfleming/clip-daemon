use crate::model::EntryKind;

pub const INSPECTION_LIMIT: usize = 64 * 1024;

pub fn classify(mime: &str, bytes: &[u8]) -> EntryKind {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "text/uri-list" | "x-special/gnome-copied-files" => return EntryKind::Files,
        "text/html" | "application/xhtml+xml" => return EntryKind::Html,
        "application/json" | "application/ld+json" => return EntryKind::Json,
        value if value.starts_with("image/") => return EntryKind::Image,
        value if !value.is_empty() && !value.starts_with("text/") => return EntryKind::Binary,
        _ => {}
    }

    let Ok(text) = std::str::from_utf8(&bytes[..bytes.len().min(INSPECTION_LIMIT)]) else {
        return EntryKind::Binary;
    };
    let value = text.trim();
    if looks_like_url(value) {
        EntryKind::Link
    } else if looks_like_json(value) {
        EntryKind::Json
    } else if looks_like_color(value) {
        EntryKind::Color
    } else {
        EntryKind::Text
    }
}

pub fn bounded_preview(bytes: &[u8], max_bytes: usize) -> String {
    let bounded = &bytes[..bytes.len().min(max_bytes)];
    let text = String::from_utf8_lossy(bounded);
    let mut result = String::with_capacity(text.len().min(256));
    let mut was_space = false;
    for character in text.chars() {
        if result.len() >= 256 {
            break;
        }
        if character.is_whitespace() {
            if !was_space && !result.is_empty() {
                result.push(' ');
            }
            was_space = true;
        } else if !character.is_control() {
            result.push(character);
            was_space = false;
        }
    }
    result.trim().to_owned()
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
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{bounded_preview, classify};
    use crate::model::EntryKind;

    #[test]
    fn authoritative_mimes_win_over_text_sniffing() {
        assert_eq!(
            classify("image/png", b"https://example.test"),
            EntryKind::Image
        );
        assert_eq!(
            classify("text/uri-list", b"file:///tmp/a"),
            EntryKind::Files
        );
        assert_eq!(
            classify("application/octet-stream", b"hello"),
            EntryKind::Binary
        );
    }

    #[test]
    fn bounded_text_inspection_refines_semantic_kinds() {
        assert_eq!(
            classify("text/plain", b"https://example.test/a"),
            EntryKind::Link
        );
        assert_eq!(classify("text/plain", br#"{"ok":true}"#), EntryKind::Json);
        assert_eq!(classify("text/plain", b"#1a2b3c"), EntryKind::Color);
        assert_eq!(classify("", b"ordinary text"), EntryKind::Text);
        assert_eq!(classify("", &[0xff, 0xfe]), EntryKind::Binary);
    }

    #[test]
    fn previews_are_single_line_and_bounded() {
        let preview = bounded_preview(b" first\n\tsecond ", 1024);
        assert_eq!(preview, "first second");
        assert!(bounded_preview(&vec![b'a'; 1024], 1024).len() <= 256);
    }
}
