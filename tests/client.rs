use std::{
    collections::HashSet,
    io::Write,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

fn route(index: usize) -> Value {
    json!({
        "consumerId": format!("view-{}", index % 2), "localId": "same-page-id",
        "generation": index / 5, "kind": "call"
    })
}

#[test]
fn routed_client_handles_large_frames_and_churn_before_draining_eof() {
    const REQUESTS: usize = 4000;
    // Exercise the real binary without ever contacting the user's clipboard service.
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
            let (method, params) = if index == 0 {
                (
                    "clipboard.selection.publishText",
                    json!({"text":"\u{1}".repeat(256 * 1024)}),
                )
            } else {
                ("clipboard.history.query", json!({"limit":200}))
            };
            let request = json!({
                "op":"call", "id":format!("request-{index}"),
                "method":method, "params":params, "route":route(index)
            });
            if index == 0 {
                assert!(
                    request.to_string().len() > shelllist_daemon_core::DEFAULT_JSONL_REQUEST_BYTES
                );
            }
            writeln!(input, "{request}").expect("write routed request");
        }
    });
    let output = child.wait_with_output().expect("drain routed replies");
    writer.join().unwrap();
    assert!(output.status.success());
    let mut seen = HashSet::new();
    for line in std::str::from_utf8(&output.stdout).unwrap().lines() {
        let reply: Value = serde_json::from_str(line).unwrap();
        assert_ne!(reply["kind"], "protocol-error");
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
        assert!(index < REQUESTS);
        assert!(seen.insert(index), "reply must be delivered exactly once");
        assert_eq!(reply["route"], route(index));
        if index == 0 {
            assert_eq!(
                reply["outcome"], "rejected-before-dispatch",
                "The large frame must reach the transport, never a real clipboard service"
            );
        }
    }
    assert_eq!(seen.len(), REQUESTS);
}
