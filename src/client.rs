use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
    task::JoinSet,
};

use crate::{
    api,
    daemon::{BUS_NAME, INTERFACE, OBJECT_PATH},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Request {
    Call {
        id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Subscribe {
        id: String,
        #[serde(default)]
        streams: Vec<String>,
    },
    Cancel {
        id: String,
        request_id: String,
    },
    Shutdown {
        id: String,
    },
}

type Output = Arc<Mutex<tokio::io::Stdout>>;

pub async fn publish(mime: &str, bytes: Vec<u8>) -> Result<()> {
    let connection = Some(
        zbus::Connection::session()
            .await
            .context("connect to session D-Bus")?,
    );
    let response = transport_call(&connection, "Publish", &(mime, bytes))
        .await
        .context("publish clipboard content")?;
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        _ => anyhow::bail!(
            response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("clip-daemon rejected the clipboard content")
                .to_owned()
        ),
    }
}

pub async fn run() -> Result<()> {
    let connection = zbus::Connection::session().await.ok();
    let output = Arc::new(Mutex::new(tokio::io::stdout()));
    if let Some(connection) = connection.clone() {
        spawn_events(connection, Arc::clone(&output));
    }
    request_loop(connection, output).await
}

async fn request_loop(connection: Option<zbus::Connection>, output: Output) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut calls = JoinSet::new();
    let mut shutdown_id = None;
    while let Some(line) = lines.next_line().await.context("read client request")? {
        reap_finished_calls(&mut calls)?;
        let Some(request) = decode_request(&line, &output).await? else {
            continue;
        };
        if let Some(id) = handle_request(request, &connection, &output, &mut calls).await? {
            shutdown_id = Some(id);
            break;
        }
    }
    // EOF and explicit shutdown are graceful: every accepted call receives a
    // response before stdout is closed. This also makes one-shot pipelines
    // reliable instead of racing detached tasks against runtime shutdown.
    drain_calls(&mut calls).await?;
    if let Some(id) = shutdown_id {
        emit(
            &output,
            &json!({"kind":"response","id":id,"ok":true,"response":{"shutdown":true}}),
        )
        .await?;
    }
    Ok(())
}

async fn decode_request(line: &str, output: &Output) -> Result<Option<Request>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            emit(
                output,
                &json!({"kind":"protocol-error","error":error.to_string()}),
            )
            .await?;
            Ok(None)
        }
    }
}

async fn handle_request(
    request: Request,
    connection: &Option<zbus::Connection>,
    output: &Output,
    calls: &mut JoinSet<Result<()>>,
) -> Result<Option<String>> {
    match request {
        Request::Call { id, method, params } => {
            calls.spawn(run_call(
                connection.clone(),
                Arc::clone(output),
                id,
                method,
                params,
            ));
        }
        Request::Subscribe { id, streams } => {
            let response = transport_call(connection, "Subscribe", &(streams,)).await;
            emit_transport(output, &id, response).await?;
        }
        Request::Cancel { id, request_id } => {
            let response = transport_call(connection, "Cancel", &(request_id.as_str(),)).await;
            emit_transport(output, &id, response).await?;
        }
        Request::Shutdown { id } => return Ok(Some(id)),
    }
    Ok(None)
}

async fn run_call(
    connection: Option<zbus::Connection>,
    output: Output,
    id: String,
    method: String,
    params: Value,
) -> Result<()> {
    let response = call(&connection, &method, params)
        .await
        .unwrap_or_else(|_| {
            api::error(
                "daemon-unavailable",
                "clip-daemon session service is unavailable".into(),
            )
        });
    emit(
        &output,
        &json!({"kind":"response","id":id,"ok":true,"response":response}),
    )
    .await
}

fn reap_finished_calls(calls: &mut JoinSet<Result<()>>) -> Result<()> {
    while let Some(result) = calls.try_join_next() {
        result.context("client call task failed")??;
    }
    Ok(())
}

async fn drain_calls(calls: &mut JoinSet<Result<()>>) -> Result<()> {
    while let Some(result) = calls.join_next().await {
        result.context("client call task failed")??;
    }
    Ok(())
}

async fn proxy(connection: &Option<zbus::Connection>) -> Result<zbus::Proxy<'_>> {
    let connection = connection.as_ref().context("session D-Bus unavailable")?;
    zbus::Proxy::new(connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("create clip-daemon proxy")
}

async fn call(connection: &Option<zbus::Connection>, method: &str, params: Value) -> Result<Value> {
    let proxy = proxy(connection).await?;
    let response: String = proxy
        .call("Call", &(method, params.to_string().as_str()))
        .await?;
    serde_json::from_str(&response).context("decode daemon response")
}

async fn transport_call<B>(
    connection: &Option<zbus::Connection>,
    method: &str,
    body: &B,
) -> Result<Value>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType + Sync,
{
    let proxy = proxy(connection).await?;
    let response: String = proxy.call(method, body).await?;
    serde_json::from_str(&response).context("decode transport response")
}

fn spawn_events(connection: zbus::Connection, output: Output) {
    tokio::spawn(async move {
        let Ok(proxy) = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).await
        else {
            return;
        };
        let Ok(mut signals) = proxy.receive_signal("Event").await else {
            return;
        };
        while let Some(message) = signals.next().await {
            let Ok((stream, event_json)) = message.body().deserialize::<(String, String)>() else {
                continue;
            };
            let event = serde_json::from_str::<Value>(&event_json)
                .unwrap_or_else(|_| json!({"raw":event_json}));
            if emit(
                &output,
                &json!({"kind":"event","stream":stream,"event":event}),
            )
            .await
            .is_err()
            {
                break;
            }
        }
    });
}

async fn emit_transport(output: &Output, id: &str, result: Result<Value>) -> Result<()> {
    let value = match result {
        Ok(response) => json!({"kind":"response","id":id,"ok":true,"response":response}),
        Err(error) => json!({"kind":"response","id":id,"ok":false,"error":error.to_string()}),
    };
    emit(output, &value).await
}

async fn emit(output: &Output, value: &Value) -> Result<()> {
    let mut output = output.lock().await;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await.context("flush client output")
}
