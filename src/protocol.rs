use serde_json::{Value, json};

pub const NAME: &str = "clip-api";
pub const VERSION: u8 = 1;

pub mod stream {
    pub const HISTORY: &str = "clipboard.history.changed";
    pub const CURRENT: &str = "clipboard.current.changed";
    pub const OPERATION: &str = "clipboard.operation";
    pub const CAPTURE: &str = "clipboard.capture.changed";
    pub const SESSION: &str = "clipboard.session";
}

pub const METHODS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "clipboard.session.begin",
        "{}",
        "session",
        Some(stream::SESSION),
    ),
    (
        "clipboard.session.end",
        r#"{"session_id":"session-opaque"}"#,
        "session",
        Some(stream::SESSION),
    ),
    (
        "clipboard.session.hidden",
        r#"{"session_id":"session-opaque"}"#,
        "session",
        Some(stream::SESSION),
    ),
    (
        "clipboard.history.query",
        r#"{"query":"","generation":1,"limit":100}"#,
        "history",
        Some(stream::HISTORY),
    ),
    (
        "clipboard.entry.details",
        r#"{"entry_id":"entry-opaque","revision":1}"#,
        "entry",
        None,
    ),
    (
        "clipboard.entry.thumbnail",
        r#"{"entry_id":"entry-opaque","revision":1,"edge":512}"#,
        "thumbnail",
        None,
    ),
    (
        "clipboard.entry.action",
        r#"{"entry_id":"entry-opaque","revision":1,"action":"copy","session_id":null}"#,
        "operation",
        Some(stream::OPERATION),
    ),
    (
        "clipboard.entry.edit.begin",
        r#"{"entry_id":"entry-opaque","revision":1}"#,
        "edit",
        Some(stream::OPERATION),
    ),
    (
        "clipboard.entry.edit.commit",
        r#"{"edit_id":"edit-opaque","value":"replacement"}"#,
        "entry",
        Some(stream::HISTORY),
    ),
    (
        "clipboard.entry.edit.cancel",
        r#"{"edit_id":"edit-opaque"}"#,
        "edit",
        None,
    ),
    (
        "clipboard.capture.setPaused",
        r#"{"paused":true}"#,
        "capture",
        Some(stream::CAPTURE),
    ),
    ("clipboard.settings.get", "{}", "settings", None),
    (
        "clipboard.settings.update",
        r#"{"max_entries":750,"max_entry_bytes":16777216}"#,
        "settings",
        None,
    ),
    ("clipboard.history.wipe.prepare", "{}", "challenge", None),
    (
        "clipboard.history.wipe.commit",
        r#"{"challenge_id":"challenge-opaque","response":"WIPE"}"#,
        "operation",
        Some(stream::OPERATION),
    ),
];

pub const STREAMS: &[(&str, &[&str])] = &[
    (
        stream::HISTORY,
        &[
            "subscribed",
            "added",
            "replaced",
            "removed",
            "reset",
            "unavailable",
        ],
    ),
    (stream::CURRENT, &["subscribed", "changed", "unavailable"]),
    (
        stream::OPERATION,
        &["started", "progress", "completed", "failed", "cancelled"],
    ),
    (stream::CAPTURE, &["subscribed", "changed", "unavailable"]),
    (
        stream::SESSION,
        &[
            "paste-prepared",
            "completed",
            "fallback",
            "target-unavailable",
            "expired",
        ],
    ),
];

pub fn registry() -> Value {
    json!({
        "protocol": NAME,
        "version": VERSION,
        "methods": METHODS.iter().map(|(name, params, response_key, stream)| json!({
            "name": name,
            "params_example": serde_json::from_str::<Value>(params).expect("valid fixture"),
            "response_key": response_key,
            "stream": stream,
        })).collect::<Vec<_>>(),
        "streams": STREAMS.iter().map(|(name, events)| json!({"name": name, "events": events})).collect::<Vec<_>>(),
    })
}

pub fn contract_fixture() -> Value {
    serde_json::from_str(include_str!("../test_support/clip-api-v1.json"))
        .expect("checked-in clip-api fixture must be valid JSON")
}

#[cfg(test)]
mod tests {
    use super::{METHODS, STREAMS, VERSION, contract_fixture, registry};
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_and_fixture_matches_registry() {
        let mut names = HashSet::new();
        assert!(METHODS.iter().all(|item| names.insert(item.0)));
        names.clear();
        assert!(STREAMS.iter().all(|item| names.insert(item.0)));
        let fixture = contract_fixture();
        assert_eq!(fixture["version"], VERSION);
        assert_eq!(fixture["registry"], registry());
    }
}
