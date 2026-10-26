//! Heavy private framing qualification. No host clipboard, capture or services.
use super::*;
const PAYLOAD_DEADLINE: Duration = Duration::from_secs(40);

async fn padded_record(peer: &mut Peer, request: Value, bytes: usize) {
    let mut prefix = request.to_string();
    assert_eq!(prefix.pop(), Some('}'));
    prefix.push_str(",\"fixture_padding\":\"");
    let suffix = "\"}\n";
    let mut remaining = bytes - prefix.len() - (suffix.len() - 1);
    peer.get_mut().write_all(prefix.as_bytes()).await.unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(chunk.len());
        peer.get_mut().write_all(&chunk[..count]).await.unwrap();
        remaining -= count;
    }
    peer.get_mut().write_all(suffix.as_bytes()).await.unwrap();
}
async fn line(peer: &mut Peer) -> String {
    let mut value = String::new();
    assert!(
        timeout(PAYLOAD_DEADLINE, peer.read_line(&mut value))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    value
}

#[tokio::test]
#[ignore = "explicit large private clipboard framing qualification"]
async fn payload_qualification_maximum_request_and_rejected_effect() {
    let backend = Arc::new(FakeBackend::with_entries(vec![entry()]));
    let fixture = Fixture::new(backend).await;
    let (stop, task) = fixture.listen(config()).await;
    let mut sender = fixture.peer().await;
    let mut peer = fixture.peer().await;
    timeout(
        PAYLOAD_DEADLINE,
        padded_record(
            &mut sender,
            call("maximum", "clipboard.history.revision", json!({})),
            config().maximum_request_bytes,
        ),
    )
    .await
    .unwrap();
    let accepted: Value = serde_json::from_str(&line(&mut sender).await).unwrap();
    assert_eq!(accepted["response"]["ok"], true);
    assert_eq!(accepted["id"], "maximum");
    timeout(
        PAYLOAD_DEADLINE,
        padded_record(
            &mut sender,
            call(
                "must-not-delete",
                "clipboard.entry.action",
                json!({"entry_id":"fixture","revision":1,"action":"delete"}),
            ),
            config().maximum_request_bytes + 1,
        ),
    )
    .await
    .unwrap();
    let rejected: Value = serde_json::from_str(&line(&mut sender).await).unwrap();
    assert_eq!(rejected["kind"], "protocol-error");
    assert!(rejected["error"].as_str().unwrap().contains("exceeds"));
    let mut events = Vec::new();
    for connection in [&mut sender, &mut peer] {
        let history = request(
            connection,
            call("still-present", "clipboard.history.query", json!({})),
            &mut events,
        )
        .await;
        assert_eq!(
            history["data"]["history"]["entries"][0]["id"], "fixture",
            "oversized effect was dispatched or healthy peer was lost"
        );
    }
    drop(sender);
    drop(peer);
    fixture.shutdown(stop, task).await;
}

async fn response_overhead() -> usize {
    let mut value = entry();
    value.entry.preview.clear();
    let api = ApiService::with_settings(
        Arc::new(FakeBackend::with_entries(vec![value])),
        crate::settings::SettingsManager::isolated(),
    );
    let response = api.dispatch("clipboard.history.query", json!({})).await;
    let mut record = shelllist_daemon_core::response_message("large", response);
    record["route"] = call("large", "clipboard.history.query", json!({}))["route"].clone();
    serde_json::to_vec(&record).unwrap().len() + 1 // writer bound includes LF
}

#[tokio::test]
#[ignore = "explicit large private clipboard framing qualification"]
async fn payload_qualification_maximum_response_and_slow_reader_owner_isolation() {
    let limit = config().maximum_response_bytes;
    let overhead = response_overhead().await;
    for scenario in ["maximum", "oversized", "stalled"] {
        let mut value = entry();
        value.entry.preview = "x".repeat(limit - overhead + usize::from(scenario == "oversized"));
        let (_finish_a, wait_a) = oneshot::channel();
        let (finish_b, wait_b) = oneshot::channel();
        let backend =
            Arc::new(FakeBackend::with_entries(vec![value]).with_operations(vec![wait_a, wait_b]));
        let fixture = Fixture::new(backend.clone()).await;
        let mut limits = config();
        if scenario == "stalled" {
            limits.write_timeout = Duration::from_millis(100);
        }
        let (stop, task) = fixture.listen(limits).await;
        let mut sender = fixture.peer().await;
        let mut peer = fixture.peer().await;
        let mut events = Vec::new();
        let params = json!({"mode":"region"});
        let a = request(
            &mut sender,
            call("a", "clipboard.capture.interactive", params.clone()),
            &mut events,
        )
        .await;
        let b = request(
            &mut peer,
            call("b", "clipboard.capture.interactive", params),
            &mut events,
        )
        .await;
        let id_a = a["data"]["operation"]["id"].as_str().unwrap();
        let id_b = b["data"]["operation"]["id"].as_str().unwrap();
        sender
            .get_mut()
            .write_all(
                format!("{}\n", call("large", "clipboard.history.query", json!({}))).as_bytes(),
            )
            .await
            .unwrap();
        if scenario == "maximum" {
            let record = line(&mut sender).await;
            assert_eq!(record.len(), limit, "exact production response ceiling");
            let response: Value = serde_json::from_str(&record).unwrap();
            assert_eq!(response["response"]["ok"], true);
            assert_eq!(
                response["response"]["data"]["history"]["entries"][0]["preview"]
                    .as_str()
                    .unwrap()
                    .len(),
                limit - overhead
            );
            drop(sender);
        }
        // Do not read from the stalled peer: real kernel backpressure must close it.
        timeout(PAYLOAD_DEADLINE, async {
            while backend.active_operations().unwrap().contains(id_a) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = request(
            &mut peer,
            call(
                "healthy",
                "clipboard.operation.status",
                json!({"operation_id":id_b}),
            ),
            &mut events,
        )
        .await;
        assert_eq!(
            status["data"]["operation"]["status"], "started",
            "{scenario}: unrelated owner was cancelled"
        );
        let foreign = request(
            &mut peer,
            call(
                "foreign",
                "clipboard.operation.status",
                json!({"operation_id":id_a}),
            ),
            &mut events,
        )
        .await;
        assert_eq!(foreign["error"]["code"], "operation-unknown");
        finish_b.send(true).unwrap();
        no_active(backend.as_ref(), id_b).await;
        drop(peer);
        fixture.shutdown(stop, task).await;
    }
}
