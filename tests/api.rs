use std::sync::Arc;

use clip_daemon::{api::ApiService, fake::FakeBackend};
use serde_json::json;

#[tokio::test]
async fn validation_reserved_methods_and_wipe_challenges_are_stable() {
    let api = ApiService::new(Arc::new(FakeBackend::default()));
    for (method, params, code) in [
        (
            "clipboard.history.query",
            json!({"limit": 0}),
            "validation-error",
        ),
        ("clipboard.entry.edit.begin", json!({}), "not-implemented"),
        ("clipboard.nope", json!({}), "unsupported-method"),
    ] {
        assert_eq!(api.dispatch(method, params).await["error"]["code"], code);
    }
    let challenge = api
        .dispatch("clipboard.history.wipe.prepare", json!({}))
        .await;
    let id = challenge["data"]["challenge"]["id"].as_str().unwrap();
    let result = api
        .dispatch(
            "clipboard.history.wipe.commit",
            json!({"challenge_id": id, "response": "WIPE"}),
        )
        .await;
    assert_eq!(result["data"]["operation"]["action"], "wipe");
}
