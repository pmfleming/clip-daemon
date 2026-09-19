use std::sync::Arc;

use clip_daemon::{
    api::ApiService,
    backend::{ClipboardBackend, HistoryQuery},
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
    let api = ApiService::with_capture(backend, Arc::new(FailedFence));
    let before = api.dispatch("clipboard.history.query", json!({})).await;
    let challenge = api
        .dispatch("clipboard.history.wipe.prepare", json!({}))
        .await;
    let result = api
        .dispatch(
            "clipboard.history.wipe.commit",
            json!({
                "challenge_id": challenge["data"]["challenge"]["id"], "response":"WIPE"
            }),
        )
        .await;
    assert_eq!(result["ok"], false);
    assert_eq!(
        api.dispatch("clipboard.history.query", json!({})).await,
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queries_observe_consistent_replacements_and_only_one_revision_wins() {
    let backend = Arc::new(FakeBackend::with_entries(vec![entry(
        "one",
        EntryKind::Text,
        Some("seed"),
    )]));
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut writers = Vec::new();
    for index in 0..8 {
        let backend = Arc::clone(&backend);
        let barrier = Arc::clone(&barrier);
        writers.push(tokio::spawn(async move {
            barrier.wait().await;
            backend
                .replace(
                    "one",
                    1,
                    "text/plain",
                    format!("replacement-{index}").as_bytes(),
                )
                .await
        }));
    }
    barrier.wait().await;
    for _ in 0..100 {
        let page = backend
            .query(HistoryQuery {
                query: String::new(),
                generation: 1,
                offset: 0,
                limit: 100,
                collapse_self_echoes: true,
            })
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        let summary = &page.entries[0];
        assert_eq!(summary.byte_size, summary.preview.len() as u64);
        assert!((1..=2).contains(&summary.revision));
        tokio::task::yield_now().await;
    }
    let mut succeeded = 0;
    for writer in writers {
        match writer.await.unwrap() {
            Ok(_) => succeeded += 1,
            Err(error) => assert_eq!(error.kind, clip_daemon::backend::BackendErrorKind::Stale),
        }
    }
    assert_eq!(succeeded, 1);
    assert_eq!(backend.revision("one").await.unwrap(), 2);
}

#[tokio::test]
async fn native_search_uses_revision_bound_owner_scoped_cursors() {
    let backend = Arc::new(FakeBackend::with_entries(vec![
        entry("one", EntryKind::Text, Some("Café one")),
        entry("two", EntryKind::Text, Some("Café two")),
        entry("three", EntryKind::Text, Some("Café three")),
    ]));
    let api = ApiService::new(backend.clone());
    let request = json!({"query":"cafee", "generation":9, "fuzzy":true, "limit":2});
    let first = api
        .dispatch_owned(
            "clipboard.history.query",
            request.clone(),
            Some(":1.1".into()),
        )
        .await;
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["data"]["history"]["total"], 3);
    assert_eq!(first["data"]["history"]["entries"][0]["id"], "one");
    let mut next = request.clone();
    next["cursor"] = first["data"]["history"]["next_cursor"].clone();
    let second = api
        .dispatch_owned("clipboard.history.query", next.clone(), Some(":1.1".into()))
        .await;
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(second["data"]["history"]["entries"][0]["id"], "three");
    assert!(second["data"]["history"]["next_cursor"].is_null());
    let wrong_owner = api
        .dispatch_owned("clipboard.history.query", next.clone(), Some(":1.2".into()))
        .await;
    assert_eq!(wrong_owner["error"]["code"], "stale-cursor");
    let restarted = ApiService::new(backend.clone())
        .dispatch_owned("clipboard.history.query", next.clone(), Some(":1.1".into()))
        .await;
    assert_eq!(restarted["error"]["code"], "stale-cursor");
    backend
        .replace("one", 1, "text/plain", b"changed")
        .await
        .unwrap();
    let changed = api
        .dispatch_owned("clipboard.history.query", next, Some(":1.1".into()))
        .await;
    assert_eq!(changed["error"]["code"], "stale-cursor");
    let fresh = api.dispatch("clipboard.history.query", request).await;
    assert_eq!(fresh["data"]["history"]["total"], 2);
}

#[tokio::test]
async fn history_pagination_is_stable() {
    let api = ApiService::new(Arc::new(FakeBackend::with_entries(vec![
        entry("one", EntryKind::Text, Some("one")),
        entry("two", EntryKind::Text, Some("two")),
        entry("three", EntryKind::Text, Some("three")),
    ])));
    let first = api
        .dispatch(
            "clipboard.history.query",
            json!({"query":"", "generation":9, "offset":0, "limit":2}),
        )
        .await;
    assert_eq!(
        first["data"]["history"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(first["data"]["history"]["next_offset"], 2);
    let second = api
        .dispatch(
            "clipboard.history.query",
            json!({"query":"", "generation":9, "offset":2, "limit":2}),
        )
        .await;
    assert_eq!(
        second["data"]["history"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(second["data"]["history"]["next_offset"].is_null());
}

#[tokio::test]
async fn bulk_delete_validates_the_selection_before_removing_entries() {
    let api = ApiService::new(Arc::new(FakeBackend::with_entries(vec![
        entry("one", EntryKind::Text, Some("one")),
        entry("two", EntryKind::Text, Some("two")),
        entry("three", EntryKind::Text, Some("three")),
    ])));
    let stale_selection = api
        .dispatch(
            "clipboard.entries.delete",
            json!({"entries":[
                {"entry_id":"one","revision":1},
                {"entry_id":"two","revision":99}
            ]}),
        )
        .await;
    assert_ne!(stale_selection["ok"], true);
    let untouched = api
        .dispatch(
            "clipboard.history.query",
            json!({"query":"", "generation":10, "offset":0, "limit":10}),
        )
        .await;
    assert_eq!(
        untouched["data"]["history"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let deleted = api
        .dispatch(
            "clipboard.entries.delete",
            json!({"entries":[
                {"entry_id":"one","revision":1},
                {"entry_id":"three","revision":1}
            ]}),
        )
        .await;
    assert_eq!(deleted["data"]["operation"]["action"], "delete-many");

    let remaining = api
        .dispatch(
            "clipboard.history.query",
            json!({"query":"", "generation":11, "offset":0, "limit":10}),
        )
        .await;
    let entries = remaining["data"]["history"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "two");

    for params in [
        json!({"entries":[]}),
        json!({"entries":[
            {"entry_id":"two","revision":1},
            {"entry_id":"two","revision":1}
        ]}),
        json!({"entries":[{"entry_id":"two","revision":99}]}),
        json!({"entries":[{"entry_id":"","revision":1}]}),
        json!({"entries":[{"entry_id":"two"}]}),
        json!({"entries":[{"entry_id":"two","revision":-1}]}),
        json!({"entries":vec![json!({"entry_id":"two","revision":1}); 5001]}),
    ] {
        assert_ne!(
            api.dispatch("clipboard.entries.delete", params).await["ok"],
            true
        );
    }
}

#[tokio::test]
async fn text_publication_uses_the_daemon_operation_boundary() {
    let api = ApiService::new(Arc::new(FakeBackend::default()));
    let response = api
        .dispatch(
            "clipboard.selection.publishText",
            json!({ "text": "WIFI:T:WPA;S:Example;P:secret;;" }),
        )
        .await;
    assert_eq!(response["ok"], true);
    assert_eq!(response["data"]["operation"]["action"], "publish");

    let empty = api
        .dispatch("clipboard.selection.publishText", json!({ "text": "" }))
        .await;
    assert_eq!(empty["error"]["code"], "validation-error");
    let unknown_field = api
        .dispatch(
            "clipboard.selection.publishText",
            json!({"text":"ok", "extra":true}),
        )
        .await;
    assert_eq!(unknown_field["error"]["code"], "validation-error");
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
            "clipboard.capture.screenshot",
            json!({"x":0,"y":0,"width":16385,"height":1}),
            "validation-error",
        ),
        (
            "clipboard.capture.screenshot",
            json!({"x":0,"y":0,"width":16384,"height":16384}),
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
    assert_eq!(committed["data"]["publication"]["published"], true);
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

    let removed_external_edit = api
        .dispatch(
            "clipboard.entry.action",
            json!({
                "entry_id":"text","revision":2,"action":"edit-external","session_id":null
            }),
        )
        .await;
    assert_eq!(removed_external_edit["error"]["code"], "validation-error");

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
