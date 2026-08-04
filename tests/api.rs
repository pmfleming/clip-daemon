use std::sync::Arc;

use clip_daemon::{
    api::ApiService,
    fake::FakeBackend,
    model::{EntryDetails, EntryKind, EntrySummary},
};
use serde_json::json;

fn entry(id: &str, kind: EntryKind, text: Option<&str>) -> EntryDetails {
    EntryDetails {
        entry: EntrySummary {
            id: id.into(),
            revision: 1,
            kind,
            mime: if kind == EntryKind::Image {
                "image/png"
            } else {
                "text/plain"
            }
            .into(),
            byte_size: 4,
            favorite: false,
            current: false,
            preview: text.unwrap_or("binary").into(),
        },
        text: text.map(str::to_owned),
        files: vec![],
        image: None,
        preview_truncated: false,
    }
}

#[tokio::test]
async fn validation_unknown_methods_and_wipe_challenges_are_stable() {
    let api = ApiService::new(Arc::new(FakeBackend::default()));
    for (method, params, code) in [
        (
            "clipboard.history.query",
            json!({"limit": 0}),
            "validation-error",
        ),
        ("clipboard.entry.edit.begin", json!({}), "validation-error"),
        (
            "clipboard.capture.screenshot",
            json!({"x":0,"y":0,"width":0,"height":720}),
            "validation-error",
        ),
        (
            "clipboard.selection.publishFiles",
            json!({"operation":"copy","paths":["relative.txt"]}),
            "validation-error",
        ),
        ("clipboard.nope", json!({}), "unsupported-method"),
    ] {
        assert_eq!(api.dispatch(method, params).await["error"]["code"], code);
    }
    let screenshot = api
        .dispatch(
            "clipboard.capture.screenshot",
            json!({"x":-1280,"y":0,"width":1280,"height":720}),
        )
        .await;
    assert_eq!(screenshot["data"]["operation"]["action"], "screenshot");

    let published = api
        .dispatch(
            "clipboard.selection.publishFiles",
            json!({"operation":"cut","paths":["/tmp/one.txt","/tmp/two.txt"]}),
        )
        .await;
    assert_eq!(published["data"]["operation"]["action"], "publish-files");

    let published = api.publish_selection("image/png", vec![1, 2, 3, 4]).await;
    assert_eq!(published["data"]["operation"]["action"], "publish");
    let empty = api.publish_selection("image/png", vec![]).await;
    assert_eq!(empty["error"]["code"], "validation-error");

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

#[tokio::test]
async fn edit_and_type_action_policy_are_daemon_enforced() {
    let backend = FakeBackend::with_entries(vec![
        entry("text", EntryKind::Text, Some("old")),
        entry("link", EntryKind::Link, Some("not a URL")),
        entry("image", EntryKind::Image, None),
        entry("binary", EntryKind::Binary, None),
    ]);
    let api = ApiService::new(Arc::new(backend));
    let begun = api
        .dispatch(
            "clipboard.entry.edit.begin",
            json!({"entry_id":"text","revision":1}),
        )
        .await;
    let edit_id = begun["data"]["edit"]["id"].as_str().unwrap();
    let committed = api
        .dispatch(
            "clipboard.entry.edit.commit",
            json!({"edit_id":edit_id,"value":"new"}),
        )
        .await;
    assert_eq!(committed["data"]["entry"]["text"], "new");
    let unsafe_paste = api
        .dispatch(
            "clipboard.entry.action",
            json!({
                "entry_id":"binary","revision":1,"action":"paste","session_id":null
            }),
        )
        .await;
    assert_eq!(unsafe_paste["error"]["code"], "validation-error");
    let malformed_url = api
        .dispatch(
            "clipboard.entry.action",
            json!({
                "entry_id":"link","revision":1,"action":"open-url","session_id":null
            }),
        )
        .await;
    assert_eq!(malformed_url["error"]["code"], "invalid-entry");

    let external_edit = api
        .dispatch(
            "clipboard.entry.action",
            json!({
                "entry_id":"text","revision":2,"action":"edit-external","session_id":null
            }),
        )
        .await;
    assert_eq!(
        external_edit["data"]["operation"]["action"],
        "edit-external"
    );

    let missing_session = api
        .dispatch(
            "clipboard.entry.action",
            json!({
                "entry_id":"image","revision":1,"action":"image-as-file","session_id":null
            }),
        )
        .await;
    assert_eq!(missing_session["error"]["code"], "validation-error");

    let session = api.dispatch("clipboard.session.begin", json!({})).await;
    let session_id = session["data"]["session"]["id"].as_str().unwrap();
    for action in ["paste", "image-as-file"] {
        let result = api
            .dispatch(
                "clipboard.entry.action",
                json!({
                    "entry_id":"image", "revision":1, "action":action,
                    "session_id":session_id
                }),
            )
            .await;
        assert_eq!(result["data"]["operation"]["action"], action);
        assert!(matches!(
            result["data"]["operation"]["status"].as_str(),
            Some("completed" | "paste-prepared")
        ));
    }
}
