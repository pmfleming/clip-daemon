use std::sync::Arc;

use clip_daemon::{
    api::ApiService,
    backend::ClipboardBackend,
    fake::FakeBackend,
    model::{EntryDetails, EntryKind, EntrySummary},
};
use serde_json::{Value, json};

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

struct TestApi(ApiService);
impl TestApi {
    fn new(entries: Vec<EntryDetails>) -> Self {
        Self(ApiService::new(Arc::new(FakeBackend::with_entries(
            entries,
        ))))
    }
    async fn ok(&self, method: &str, params: Value) -> Value {
        let mut response = self
            .0
            .dispatch(&format!("clipboard.{method}"), params)
            .await;
        assert_eq!(response["ok"], true, "{method}: {response}");
        response["data"].take()
    }
    async fn rejects(&self, method: &str, params: Value, code: &str) {
        let response = self
            .0
            .dispatch(&format!("clipboard.{method}"), params)
            .await;
        assert_eq!(response["ok"], false, "{method}: {response}");
        assert_eq!(response["error"]["code"], code, "{method}: {response}");
    }
    async fn history(&self, params: Value) -> Value {
        self.ok("history.query", params).await["history"].take()
    }
}

#[tokio::test]
async fn wipe_requires_a_verified_capture_fence_before_deleting_anything() {
    struct FailedFence;
    #[async_trait::async_trait]
    impl clip_daemon::capture::CaptureControl for FailedFence {
        async fn set_paused(&self, _: bool, _: u64) -> Result<(), String> {
            Err("uncertain capture submission".into())
        }
        async fn is_paused(&self) -> Result<bool, String> {
            Err("uncertain".into())
        }
    }
    let backend = Arc::new(FakeBackend::with_entries(vec![entry(
        "keep",
        EntryKind::Text,
        Some("keep"),
    )]));
    let api = TestApi(ApiService::with_capture(backend, Arc::new(FailedFence)));
    let before = api.history(json!({})).await;
    let challenge = api.ok("history.wipe.prepare", json!({})).await;
    api.rejects(
        "history.wipe.commit",
        json!({
            "challenge_id": challenge["challenge"]["id"], "response":"WIPE"
        }),
        "settings-error",
    )
    .await;
    assert_eq!(api.history(json!({})).await, before);
}

#[tokio::test]
async fn native_search_uses_revision_bound_owner_scoped_cursors() {
    let backend = Arc::new(FakeBackend::with_entries(vec![
        entry("one", EntryKind::Text, Some("Café one")),
        entry("two", EntryKind::Text, Some("Café two")),
        entry("three", EntryKind::Text, Some("Café three")),
    ]));
    let api = TestApi(ApiService::new(backend.clone()));
    let plain = api.history(json!({"limit":3})).await;
    for query in ["", " \t\n", "\u{2003}"] {
        let blank = api
            .history(json!({"query":query,"fuzzy":true,"limit":3}))
            .await;
        assert_eq!(blank["entries"], plain["entries"]);
    }
    let request = || json!({"query":"cafee","generation":9,"fuzzy":true,"limit":2});
    let first = api
        .0
        .dispatch_owned("clipboard.history.query", request(), Some(":1.1".into()))
        .await;
    assert_eq!(first["ok"], true, "{first}");
    let first = &first["data"]["history"];
    assert_eq!(first["total"], 3);
    assert_eq!(first["entries"][0]["id"], "one");
    let next = || {
        let mut value = request();
        value["cursor"] = first["next_cursor"].clone();
        value
    };
    let second = api
        .0
        .dispatch_owned("clipboard.history.query", next(), Some(":1.1".into()))
        .await;
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(second["data"]["history"]["entries"][0]["id"], "three");
    assert!(second["data"]["history"]["next_cursor"].is_null());
    for owner in [None, Some(":1.2".into())] {
        let denied = api
            .0
            .dispatch_owned("clipboard.history.query", next(), owner)
            .await;
        assert_eq!(denied["error"]["code"], "stale-cursor");
    }
    backend
        .replace("one", 1, "text/plain", b"changed")
        .await
        .unwrap();
    let changed = api
        .0
        .dispatch_owned("clipboard.history.query", next(), Some(":1.1".into()))
        .await;
    assert_eq!(changed["error"]["code"], "stale-cursor");
    assert_eq!(api.history(request()).await["total"], 2);
}

#[tokio::test]
async fn bulk_delete_validates_the_selection_before_removing_entries() {
    let api = TestApi::new(
        ["one", "two", "three"]
            .map(|id| entry(id, EntryKind::Text, Some(id)))
            .into(),
    );
    let before = api.history(json!({})).await;
    for (entries, code) in [
        (json!([]), "validation-error"),
        (
            json!([{"entry_id":"two","revision":1},{"entry_id":"two","revision":1}]),
            "validation-error",
        ),
        (
            json!([{"entry_id":"one","revision":1},{"entry_id":"two","revision":99}]),
            "stale-action",
        ),
        (
            json!([{"entry_id":"missing","revision":1}]),
            "entry-not-found",
        ),
        (json!([{"entry_id":"","revision":1}]), "validation-error"),
        (json!([{"entry_id":"two"}]), "validation-error"),
        (
            json!([{"entry_id":"two","revision":-1}]),
            "validation-error",
        ),
        (
            json!(vec![json!({"entry_id":"two","revision":1}); 5001]),
            "validation-error",
        ),
    ] {
        api.rejects("entries.delete", json!({"entries":entries}), code)
            .await;
        assert_eq!(api.history(json!({})).await, before);
    }
    let page = api
        .history(json!({"generation":10,"offset":1,"limit":1}))
        .await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["id"], "two");
    assert_eq!(page["next_offset"], 2);
    let deleted = api
        .ok(
            "entries.delete",
            json!({"entries":[
                {"entry_id":"one","revision":1},{"entry_id":"three","revision":1}
            ]}),
        )
        .await;
    assert_eq!(deleted["operation"]["action"], "delete-many");
    let remaining = api.history(json!({})).await;
    assert_eq!(remaining["entries"].as_array().unwrap().len(), 1);
    assert_eq!(remaining["entries"][0]["id"], "two");
    assert!(remaining["next_offset"].is_null());
}

#[tokio::test]
async fn validation_publication_and_wipe_challenges_are_stable() {
    let api = TestApi::new(vec![]);
    let empty = api.history(json!({"fuzzy":true})).await;
    assert_eq!(empty["entries"], json!([]));
    assert_eq!(empty["total"], 0);
    for (method, params) in [
        ("selection.publishText", json!({"text":""})),
        ("selection.publishText", json!({"text":"ok","extra":true})),
        ("entry.edit.begin", json!({})),
        ("capture.interactive", json!({"mode":"window"})),
        (
            "capture.interactive",
            json!({"mode":"region","command":"evil"}),
        ),
        ("history.query", json!({"limit":0})),
        ("history.query", json!({"limit":201})),
        ("history.query", json!({"query":"x".repeat(4097)})),
        ("history.query", json!({"fuzzy":true,"offset":1})),
        ("history.query", json!({"cursor":"unused"})),
        (
            "history.query",
            json!({"fuzzy":true,"query":"é".repeat(257)}),
        ),
    ] {
        api.rejects(method, params, "validation-error").await;
    }
    for method in ["nope", "entry.nope", "session.nope", "history.wipe.nope"] {
        api.rejects(method, json!({}), "unsupported-method").await;
    }
    api.rejects(
        "settings.update",
        json!({"max_entries":0}),
        "settings-error",
    )
    .await;
    api.rejects(
        "history.query",
        json!({"fuzzy":true,"cursor":"invalid"}),
        "stale-cursor",
    )
    .await;
    for (width, height) in [(0, 720), (16385, 1), (16384, 16384)] {
        api.rejects(
            "capture.screenshot",
            json!({"x":0,"y":0,"width":width,"height":height}),
            "validation-error",
        )
        .await;
    }
    for params in [
        json!({"operation":"copy","paths":["relative.txt"]}),
        json!({"operation":"cut","paths":[]}),
        json!({"operation":"invalid","paths":["/tmp/file"]}),
        json!({"operation":"copy","paths":vec!["/tmp/file";101]}),
    ] {
        api.rejects("selection.publishFiles", params, "validation-error")
            .await;
    }
    // No sensitive-source capability: never fall back to ordinary publication.
    let api = TestApi::new(vec![entry("keep", EntryKind::Text, Some("keep"))]);
    let before = api.history(json!({})).await;
    api.rejects(
        "selection.publishSensitive",
        json!({"text":"001234","expiry_seconds":0}),
        "backend-unavailable",
    )
    .await;
    for params in [
        json!({"text":"001234","expiry_seconds":5}),
        json!({"text":"001234","expiry_seconds":301}),
        json!({"text":"1".repeat(257),"expiry_seconds":30}),
        json!({"text":"001234","expiry_seconds":30,"persist":true}),
    ] {
        api.rejects("selection.publishSensitive", params, "validation-error")
            .await;
    }
    assert_eq!(api.history(json!({})).await, before);
    for (method, params, action) in [
        (
            "capture.screenshot",
            json!({"x":-1280,"y":0,"width":1280,"height":720}),
            "screenshot",
        ),
        (
            "selection.publishFiles",
            json!({"operation":"cut","paths":["/tmp/one.txt","/tmp/two.txt"]}),
            "publish-files",
        ),
        (
            "selection.publishText",
            json!({"text":"clipboard text"}),
            "publish",
        ),
    ] {
        assert_eq!(api.ok(method, params).await["operation"]["action"], action);
    }
    let published = api.0.publish_selection("image/png", vec![1, 2, 3, 4]).await;
    assert_eq!(published["ok"], true);
    assert_eq!(published["data"]["operation"]["action"], "publish");
    assert_eq!(
        api.0.publish_selection("image/png", vec![]).await["error"]["code"],
        "validation-error"
    );
    let challenge = api.ok("history.wipe.prepare", json!({})).await;
    let id = &challenge["challenge"]["id"];
    let wipe = |response| json!({"challenge_id":id,"response":response});
    api.rejects("history.wipe.commit", wipe("NO"), "validation-error")
        .await;
    assert_eq!(
        api.ok("history.wipe.commit", wipe("WIPE")).await["operation"]["action"],
        "wipe"
    );
    api.rejects("history.wipe.commit", wipe("WIPE"), "stale-action")
        .await;
}

#[tokio::test]
async fn edit_and_type_action_policy_are_daemon_enforced() {
    let mut truncated = entry("truncated", EntryKind::Link, Some("not a URL"));
    truncated.preview_truncated = true;
    let api = TestApi::new(vec![
        truncated,
        entry("text", EntryKind::Text, Some("old")),
        entry("link", EntryKind::Link, Some("not a URL")),
        entry("image", EntryKind::Image, None),
        entry("binary", EntryKind::Binary, None),
    ]);
    let begun = api
        .ok("entry.edit.begin", json!({"entry_id":"text","revision":1}))
        .await;
    let edit_id = &begun["edit"]["id"];
    let committed = api
        .ok(
            "entry.edit.commit",
            json!({"edit_id":edit_id,"value":"new"}),
        )
        .await;
    assert_eq!(committed["entry"]["text"], "new");
    assert_eq!(committed["publication"]["published"], true);
    let details = api
        .ok("entry.details", json!({"entry_id":"text","revision":2}))
        .await;
    assert_eq!(details["entry"]["text"], "new");
    for (entry_id, action, revision, code) in [
        ("binary", "paste", 1, "validation-error"),
        ("text", "not-an-action", 2, "validation-error"),
        ("text", "open-file", 2, "validation-error"),
        ("link", "open-url", 1, "invalid-entry"),
        ("truncated", "open-url", 1, "invalid-entry"),
        ("image", "image-as-file", 1, "validation-error"),
        ("text", "copy", 1, "stale-entry"),
        ("missing", "copy", 1, "entry-not-found"),
    ] {
        api.rejects(
            "entry.action",
            json!({"entry_id":entry_id,"revision":revision,"action":action}),
            code,
        )
        .await;
    }
    let incomplete = api
        .0
        .dispatch(
            "clipboard.entry.action",
            json!({"entry_id":"truncated","revision":1,"action":"open-url"}),
        )
        .await;
    assert_eq!(
        incomplete["error"]["message"],
        "Clipboard text is too large to launch safely"
    );
    for id in ["binary", "truncated"] {
        api.rejects(
            "entry.edit.begin",
            json!({"entry_id":id,"revision":1}),
            "edit-error",
        )
        .await;
    }
    api.rejects(
        "entry.thumbnail",
        json!({"entry_id":"image","revision":1}),
        "entry-not-found",
    )
    .await;
    let edit = api
        .ok("entry.edit.begin", json!({"entry_id":"text","revision":2}))
        .await;
    for cancelled in [true, false] {
        let result = api
            .ok("entry.edit.cancel", json!({"edit_id":edit["edit"]["id"]}))
            .await;
        assert_eq!(result["edit"]["cancelled"], cancelled);
    }
    let session = api.ok("session.begin", json!({})).await;
    let session_id = &session["session"]["id"];
    for action in ["paste", "image-as-file"] {
        let result = api
            .ok(
                "entry.action",
                json!({"entry_id":"image","revision":1,"action":action,"session_id":session_id}),
            )
            .await;
        assert_eq!(result["operation"]["action"], action);
        assert!(matches!(
            result["operation"]["status"].as_str(),
            Some("completed" | "paste-prepared")
        ));
    }
    api.ok("session.end", json!({"session_id":session_id}))
        .await;
    api.rejects(
        "session.hidden",
        json!({"session_id":session_id}),
        "stale-target",
    )
    .await;
}
