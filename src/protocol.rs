use serde_json::Value;

pub const NAME: &str = "clip-api";
pub const VERSION: u8 = 1;

pub mod stream {
    pub const HISTORY: &str = "clipboard.history.changed";
    pub const CURRENT: &str = "clipboard.current.changed";
    pub const OPERATION: &str = "clipboard.operation";
    pub const CAPTURE: &str = "clipboard.capture.changed";
    pub const SESSION: &str = "clipboard.session";
}

pub const METHODS: &[&str] = &[
    "clipboard.session.begin",
    "clipboard.session.end",
    "clipboard.session.hidden",
    "clipboard.history.query",
    "clipboard.entry.details",
    "clipboard.entry.thumbnail",
    "clipboard.entry.action",
    "clipboard.entry.edit.begin",
    "clipboard.entry.edit.commit",
    "clipboard.entry.edit.cancel",
    "clipboard.capture.setPaused",
    "clipboard.settings.get",
    "clipboard.settings.update",
    "clipboard.history.wipe.prepare",
    "clipboard.history.wipe.commit",
];

pub const STREAMS: &[&str] = &[
    stream::HISTORY,
    stream::CURRENT,
    stream::OPERATION,
    stream::CAPTURE,
    stream::SESSION,
];

pub fn registry() -> Value {
    contract_fixture()["registry"].clone()
}

pub fn contract_fixture() -> Value {
    serde_json::from_str(include_str!("../test_support/clip-api-v1.json"))
        .expect("checked-in clip-api fixture must be valid JSON")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{METHODS, STREAMS, VERSION, Value, contract_fixture};

    #[test]
    fn names_are_unique_and_fixture_matches_registry() {
        assert_unique(METHODS);
        assert_unique(STREAMS);
        let fixture = contract_fixture();
        assert_eq!(fixture["version"], VERSION);
        assert_eq!(fixture_names(&fixture, "methods"), METHODS);
        assert_eq!(fixture_names(&fixture, "streams"), STREAMS);
    }

    fn assert_unique(values: &[&str]) {
        let mut names = HashSet::new();
        assert!(values.iter().all(|value| names.insert(*value)));
    }

    fn fixture_names<'a>(fixture: &'a Value, section: &str) -> Vec<&'a str> {
        fixture["registry"][section]
            .as_array()
            .expect("fixture registry section must be an array")
            .iter()
            .map(|item| {
                item["name"]
                    .as_str()
                    .expect("fixture name must be a string")
            })
            .collect()
    }
}
