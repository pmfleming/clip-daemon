use clipboard_history_client_sdk::watcher_utils::best_target::BestMimeTypeFinder;
use clipboard_history_core::protocol::MimeType;

pub(super) const MAX_OFFERS: usize = 64;
pub(super) const MAX_SEATS: usize = 16;

#[derive(Default)]
pub(super) struct OfferedMimes {
    finder: BestMimeTypeFinder<String>,
    count: usize,
    bytes: usize,
    rejected: bool,
}

impl OfferedMimes {
    pub fn add(&mut self, value: String) {
        if self.rejected {
            return;
        }
        self.count += 1;
        self.bytes += value.len();
        if value.is_empty()
            || self.count > 64
            || self.bytes > 8192
            || value.len() > 96
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || !byte.is_ascii())
        {
            self.rejected = true;
            return;
        }
        match MimeType::from(&value) {
            Ok(mime) => self.finder.add_mime(&mime, value),
            Err(_) => self.rejected = true,
        }
    }

    pub fn next(&mut self) -> Option<String> {
        if self.rejected {
            None
        } else {
            self.finder.pop_best()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OfferedMimes;

    #[test]
    fn overflow_never_truncates_away_sensitive_markers() {
        for count in [1, 63, 64, 65] {
            let mut mimes = OfferedMimes::default();
            for _ in 0..count {
                mimes.add("text/plain".into());
            }
            mimes.add("x-kde-passwordManagerHint".into());
            assert!(mimes.next().is_none());
        }
        for bad in ["a".repeat(97), "text/plain\n".into(), "text/é".into()] {
            let mut mimes = OfferedMimes::default();
            mimes.add("text/plain".into());
            mimes.add(bad);
            assert!(mimes.next().is_none());
        }
    }

    #[test]
    fn daemon_chooser_passes_every_baseline_fixture() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../test_support/capture-mime-contract.json"
        ))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let mut mimes = OfferedMimes::default();
            for mime in fixture["offers"].as_array().unwrap() {
                mimes.add(mime.as_str().unwrap().to_owned());
            }
            let selected: Vec<_> = std::iter::from_fn(|| mimes.next()).collect();
            assert_eq!(
                serde_json::json!(selected),
                fixture["expected"],
                "{}",
                fixture["name"]
            );
        }
    }
}
