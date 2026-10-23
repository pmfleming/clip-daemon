use crate::{model::HistoryPage, protocol};
use serde_json::json;
#[test]
fn frontend_contract_history_uses_the_live_serde_dto() {
    let f = protocol::contract_fixture().unwrap();
    let page: HistoryPage = serde_json::from_value(f["history_example"].clone()).unwrap();
    assert_eq!(serde_json::to_value(page).unwrap(), f["history_example"]);
    for path in [
        "/revision",
        "/entries/0/byte_size",
        "/entries/0/kind",
        "/has_more",
    ] {
        let mut bad = f["history_example"].clone();
        *bad.pointer_mut(path).unwrap() = json!({});
        assert!(serde_json::from_value::<HistoryPage>(bad).is_err());
    }
    for name in [
        "clipboard.capture.setPaused",
        "clipboard.selection.publishFiles",
        "clipboard.entries.delete",
    ] {
        assert!(
            f["registry"]["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == name)
        );
    }
    assert!(
        f["registry"]["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "clipboard.operation"
                && v["events"].as_array().unwrap().contains(&json!("progress")))
    );
}
