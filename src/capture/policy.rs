pub(super) const MAX_OFFERS: usize = 64;
pub(super) const MAX_SEATS: usize = 16;

#[derive(Default)]
pub(super) struct OfferedMimes {
    choices: [Option<String>; 6],
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
        if value == "application/x-shelllist-sensitive" || value == "x-kde-passwordManagerHint" {
            self.rejected = true;
            return;
        }
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
        // Preserve the established preference/fallback contract without an SDK.
        let essence = value.split(';').next().unwrap_or(&value);
        let rank = match essence {
            "text/plain" | "UTF8_STRING" | "STRING" | "TEXT" => 0,
            value if value.starts_with("image/") => 1,
            value if value.starts_with("x-special/") => 2,
            "application/x-gnome-copied-files" => 2,
            "chromium/x-web-custom-data" => 3,
            value if value.starts_with("chromium/x-internal") => return,
            value if value.starts_with("text/") => 4,
            value if value.contains('/') && value.starts_with(|c: char| c.is_ascii_lowercase()) => {
                5
            }
            _ => return,
        };
        let replace = self.choices[rank]
            .as_ref()
            .is_none_or(|previous| previous.contains(';') && !value.contains(';'));
        if replace {
            self.choices[rank] = Some(value);
        }
    }

    pub fn next(&mut self) -> Option<String> {
        if self.rejected {
            None
        } else {
            self.choices.iter_mut().find_map(Option::take)
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
        for bad in [
            "a".repeat(97),
            "text/plain\n".into(),
            "text/é".into(),
            "application/x-shelllist-sensitive".into(),
        ] {
            let mut mimes = OfferedMimes::default();
            mimes.add("text/plain".into());
            mimes.add(bad);
            assert!(mimes.next().is_none());
        }
    }

    #[test]
    fn representation_groups_preserve_priority_and_prefer_parameterless() {
        let mut mimes = OfferedMimes::default();
        for mime in [
            "text/html;charset=utf-8",
            "text/uri-list",
            "image/png;profile=sRGB",
            "image/jpeg",
            "text/plain",
            "x-special/gnome-copied-files",
            "chromium/x-internal-other",
        ] {
            mimes.add(mime.into());
        }
        assert_eq!(
            std::iter::from_fn(|| mimes.next()).collect::<Vec<_>>(),
            [
                "text/plain",
                "image/jpeg",
                "x-special/gnome-copied-files",
                "text/uri-list"
            ]
        );
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
