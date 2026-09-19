//! Baseline contract for the SDK MIME policy used by the current watcher.
//! Keep these fixtures when the collector moves into the daemon.
use clipboard_history_client_sdk::watcher_utils::best_target::BestMimeTypeFinder;
use clipboard_history_core::protocol::MimeType;
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    offers: Vec<String>,
    expected: Vec<String>,
}

#[test]
fn capture_mime_priority_and_whole_offer_exclusion_match_baseline() {
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../test_support/capture-mime-contract.json")).unwrap();
    for fixture in fixtures {
        let mut finder = BestMimeTypeFinder::default();
        for offered in &fixture.offers {
            let mime = MimeType::from(offered).unwrap();
            finder.add_mime(&mime, offered.clone());
        }
        let mut selected = Vec::new();
        while let Some(mime) = finder.pop_best() {
            selected.push(mime);
        }
        assert_eq!(selected, fixture.expected, "{}", fixture.name);
    }
}
