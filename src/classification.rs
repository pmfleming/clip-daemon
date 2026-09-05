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
                .filter(|character| preview_character_is_visible(*character))
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_utf8(normalized, PREVIEW_LIMIT)
}

fn preview_character_is_visible(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
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
    fn classification_respects_authoritative_mimes_before_refining_text() {
        let cases: [(&str, &[u8], EntryKind); 8] = [
            ("image/png", b"https://example.test", EntryKind::Image),
            ("text/uri-list", b"file:///tmp/a", EntryKind::Files),
            ("application/octet-stream", b"hello", EntryKind::Binary),
            ("text/plain", b"https://example.test/a", EntryKind::Link),
            ("text/plain", br#"{"ok":true}"#, EntryKind::Json),
            ("text/plain", b"#1a2b3c", EntryKind::Color),
            ("", b"ordinary text", EntryKind::Text),
            ("", &[0xff, 0xfe], EntryKind::Binary),
        ];
        for (mime, bytes, expected) in cases {
            assert_eq!(classify(mime, bytes), expected, "{mime}: {bytes:?}");
        }
    }

    #[test]
    fn previews_are_bounded_single_line_and_strip_spoofing_characters() {
        for (input, expected) in [
            (" first\n\tsecond ", "first second"),
            (
                "safe\u{202e}gpj.exe zero\u{200b}width عربي",
                "safegpj.exe zerowidth عربي",
            ),
        ] {
            assert_eq!(bounded_preview(input.as_bytes(), 1024), expected);
        }
        assert!(bounded_preview(&vec![b'a'; 1024], 1024).len() <= 256);
    }
}
