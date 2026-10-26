use super::*;
use crate::{
    backend::{BackendMutation, ClipboardBackend},
    fake::{DetailsGate, FakeBackend},
    ringboard::RingboardBackend,
};

#[tokio::test]
async fn quiet_terminal_loss_recovers_by_owned_read_without_restarting_work() {
    let backend = Arc::new(RingboardBackend::default());
    let api = ApiService::new(backend.clone());
    let (finish, finished) = tokio::sync::oneshot::channel();
    let operation = backend.test_operation(finished).unwrap();
    api.track_operation(operation.clone(), Some("a".into()));
    let params = || json!({"operation_id":operation.id});
    let first = api
        .dispatch_owned("clipboard.operation.status", params(), Some("a".into()))
        .await;
    assert_eq!(first["data"]["operation"]["status"], "started");
    for owner in [None, Some("b".into())] {
        assert_eq!(
            api.dispatch_owned("clipboard.operation.status", params(), owner)
                .await["error"]["code"],
            "operation-unknown"
        );
    }
    finish.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while backend.active_operations().unwrap().contains(&operation.id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // No event observer was running: the retained native result is authoritative.
    let terminal = api
        .dispatch_owned("clipboard.operation.status", params(), Some("a".into()))
        .await;
    assert_eq!(terminal["data"]["operation"]["status"], "completed");
    assert!(
        terminal["data"]["operation"]["_publication"]["revision"]
            .as_u64()
            .unwrap()
            > first["data"]["operation"]["_publication"]["revision"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        api.dispatch_owned("clipboard.operation.status", params(), Some("a".into()))
            .await,
        terminal
    );
    assert!(backend.active_operations().unwrap().is_empty());
    api.operations
        .lock()
        .unwrap()
        .records
        .get_mut(&operation.id)
        .unwrap()
        .updated = Instant::now() - operations::RETENTION;
    assert_eq!(
        api.dispatch_owned("clipboard.operation.status", params(), Some("a".into()))
            .await["error"]["code"],
        "operation-unknown"
    );
    assert_eq!(
        api.dispatch_owned(
            "clipboard.operation.status",
            json!({"operation_id":"", "retry":true}),
            Some("a".into())
        )
        .await["error"]["code"],
        "validation-error"
    );
}

#[tokio::test]
async fn history_reads_are_token_sealed_and_share_invalidation_watermarks() {
    let summary = protocol::contract_fixture().unwrap()["history_example"]["entries"][0].clone();
    let entry = serde_json::from_value(
        json!({"entry":summary,"text":"test","files":[],"image":null,"preview_truncated":false}),
    )
    .unwrap();
    let gate = Arc::new(DetailsGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(1),
    });
    let mut backend = FakeBackend::with_entries(vec![entry]);
    backend.query_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let api = Arc::new(ApiService::new(backend.clone()));
    let old = api.dispatch("clipboard.history.query", json!({})).await;
    assert_eq!(old["ok"], true);
    gate.entered.notified().await;
    assert_eq!(
        old["data"]["_history"],
        api.history_observation().await.unwrap()["_history"]
    );
    let queried = api.clone();
    let query =
        tokio::spawn(async move { queried.dispatch("clipboard.history.query", json!({})).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    backend
        .mutate("entry-example", Some(1), BackendMutation::Remove)
        .await
        .unwrap();
    gate.release.add_permits(1);
    assert_eq!(query.await.unwrap()["error"]["code"], "stale-history");
    let current = api.history_observation().await.unwrap();
    assert_eq!(current["history_revision"], "0"); // content tokens are not counters
    assert!(
        current["_history"]["revision"].as_u64().unwrap()
            > old["data"]["_history"]["revision"].as_u64().unwrap()
    );
    let revision = api.dispatch("clipboard.history.revision", json!({})).await;
    assert_eq!(revision["data"]["_history"], current["_history"]);
    assert_eq!(revision["data"]["snapshot_revision"], "0");
}
