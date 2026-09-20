use std::{
    io::Write,
    process::{Command, Stdio},
};

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
