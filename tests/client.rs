use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn jsonl_client_drains_calls_after_stdin_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_clip-daemon"))
        .arg("client")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start JSONL client");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(
            br#"{"op":"call","id":"eof-call","method":"clipboard.settings.get","params":{}}
"#,
        )
        .expect("write one-shot request");

    let output = child.wait_with_output().expect("wait for JSONL client");
    assert!(output.status.success());
    let lines = std::str::from_utf8(&output.stdout).expect("UTF-8 JSONL responses");
    let response = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSONL response"))
        .find(|message| message["kind"] == "response" && message["id"] == "eof-call")
        .expect("response for the accepted request");
    assert_eq!(response["kind"], "response");
}
