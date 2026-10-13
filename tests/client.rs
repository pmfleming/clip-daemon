use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn escaped_clipboard_text_can_exceed_the_small_rpc_frame_default() {
    use serde_json::{Value, json};
    let request = json!({"op":"call", "id":"large-text", "method":"clipboard.selection.publishText",
        "params":{"text":"\u{1}".repeat(256 * 1024)},
        "route":{"consumerId":"clipboard", "localId":"publish", "generation":1, "kind":"call"}});
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(encoded.len() > shelllist_daemon_core::DEFAULT_JSONL_REQUEST_BYTES);
    let mut child = Command::new(env!("CARGO_BIN_EXE_clip-daemon"))
        .arg("client")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-shelllist-bulk-bridge-test",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        writeln!(input, "{encoded}").unwrap();
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success());
    let messages: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !messages
            .iter()
            .any(|value| value["kind"] == "protocol-error")
    );
    let response = messages
        .iter()
        .find(|value| value["kind"] == "response" && value["id"] == "large-text")
        .unwrap();
    assert_eq!(response["route"], request["route"]);
    assert_eq!(
        response["response"]["error"]["code"], "daemon-unavailable",
        "The frame must reach the transport, never a real clipboard service"
    );
}

#[test]
fn routed_client_churn_preserves_addresses_and_drains_after_eof() {
    use serde_json::{Value, json};
    use std::collections::HashSet;

    const REQUESTS: usize = 4000;
    // Exercise the real binary and framing without contacting the user's
    // clipboard service. API-unavailable and overload replies must both route.
    let mut child = Command::new(env!("CARGO_BIN_EXE_clip-daemon"))
        .arg("client")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-shelllist-bridge-test",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start routed JSONL client");
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        for index in 0..REQUESTS {
            let request = json!({
                "op": "call", "id": format!("request-{index}"),
                "method": "clipboard.history.query", "params": { "limit": 200 },
                "route": { "consumerId": format!("view-{}", index % 2),
                    "localId": "same-page-id", "generation": index / 5, "kind": "call" }
            });
            writeln!(input, "{request}").expect("write routed request");
        }
    });
    let output = child.wait_with_output().expect("drain routed replies");
    writer.join().unwrap();
    assert!(output.status.success());
    let mut seen = HashSet::new();
    for line in std::str::from_utf8(&output.stdout).unwrap().lines() {
        let reply: Value = serde_json::from_str(line).unwrap();
        if reply["kind"] != "response" {
            continue;
        }
        let index: usize = reply["id"]
            .as_str()
            .unwrap()
            .strip_prefix("request-")
            .unwrap()
            .parse()
            .unwrap();
        assert!(seen.insert(index), "reply must be delivered exactly once");
        assert_eq!(
            reply["route"],
            json!({
                "consumerId": format!("view-{}", index % 2), "localId": "same-page-id",
                "generation": index / 5, "kind": "call"
            })
        );
    }
    assert_eq!(seen.len(), REQUESTS);
}
