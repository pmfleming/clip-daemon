use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use shelllist_daemon_core::{ClientRequest, DaemonEndpoint};
use shelllist_daemon_tokio::{
    BasicCorrelation, JsonDbusClient, OutputCommand, OutputHandle, spawn_output_actor,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    task::{JoinHandle, JoinSet},
};

use crate::daemon::{self, BUS_NAME, INTERFACE, OBJECT_PATH};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("clip-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);
const MAX_IN_FLIGHT_REQUESTS: usize = 64;

pub async fn publish(mime: &str, bytes: Vec<u8>) -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("connect to session D-Bus")?;
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("create clip-daemon proxy")?;
    let response: String = proxy
        .call("Publish", &(mime, bytes))
        .await
        .context("publish clipboard content")?;
    let response: Value =
        serde_json::from_str(&response).context("decode clipboard publish response")?;
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
    let dbus = JsonDbusClient::session(ENDPOINT).await.ok();
    let (output, output_task) = spawn_output_actor(BasicCorrelation, 64, 32);
    let watchers = dbus.as_ref().map(|client| {
        [
            spawn_event_forwarder(client.clone(), output.clone()),
            spawn_owner_watcher(client.clone(), output.clone()),
        ]
    });
    let mut calls = JoinSet::new();
    let shutdown_id = request_loop(dbus.as_ref(), &output, &mut calls).await?;
    drain_calls(&mut calls).await;
    cancel_active(dbus.as_ref(), &output).await;
    watchers.iter().flatten().for_each(JoinHandle::abort);
    if let Some(id) = shutdown_id {
        output.send(OutputCommand::Shutdown(id)).await?;
    }
    drop(output);
    output_task
        .await
        .context("join JSONL output task")?
        .context("run JSONL output task")
}

async fn request_loop(
    dbus: Option<&JsonDbusClient>,
    output: &OutputHandle,
    calls: &mut JoinSet<()>,
) -> Result<Option<String>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("read JSONL request")? {
        let Some(request) = decode_request(&line, output).await? else {
            continue;
        };
        if let ClientRequest::Shutdown { id } = request {
            return Ok(Some(id));
        }
        wait_for_request_slot(calls).await;
        calls.spawn(run_request(dbus.cloned(), output.clone(), request));
        reap_finished(calls);
    }
    Ok(None)
}

async fn decode_request(line: &str, output: &OutputHandle) -> Result<Option<ClientRequest>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            output
                .send(OutputCommand::ProtocolError(error.to_string()))
                .await?;
            Ok(None)
        }
    }
}

async fn run_request(dbus: Option<JsonDbusClient>, output: OutputHandle, request: ClientRequest) {
    let (id, result, cancelled_request_id) = match request {
        ClientRequest::Call { id, method, params } => {
            let response = call(dbus.as_ref(), &method, params).await;
            (id, Ok(response), None)
        }
        ClientRequest::Subscribe { id, streams } => (
            id,
            transport(dbus.as_ref(), Transport::Subscribe(streams)).await,
            None,
        ),
        ClientRequest::Cancel { id, request_id } => {
            let result = transport(dbus.as_ref(), Transport::Cancel(&request_id)).await;
            let cancelled = result.as_ref().ok().map(|_| request_id);
            (id, result, cancelled)
        }
        ClientRequest::Shutdown { .. } => return,
    };
    let _ = output
        .send(OutputCommand::Response {
            id,
            result,
            cancelled_request_id,
        })
        .await;
}

async fn call(dbus: Option<&JsonDbusClient>, method: &str, params: Value) -> Value {
    let result = match dbus {
        Some(client) => client.call(method, params).await,
        None => Err(anyhow!("session D-Bus unavailable")),
    };
    result.unwrap_or_else(|_| daemon::unavailable_response())
}

enum Transport<'a> {
    Subscribe(Vec<String>),
    Cancel(&'a str),
}

async fn transport(dbus: Option<&JsonDbusClient>, request: Transport<'_>) -> Result<Value, String> {
    match (dbus, request) {
        (Some(client), Transport::Subscribe(streams)) => client.subscribe(streams).await,
        (Some(client), Transport::Cancel(id)) => client.cancel_json(id).await,
        (None, _) => Err(anyhow!("session D-Bus unavailable")),
    }
    .map_err(|error| error.to_string())
}

fn spawn_event_forwarder(dbus: JsonDbusClient, output: OutputHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = dbus.forward_events(&output).await {
            let _ = output
                .send(OutputCommand::TransportError(error.to_string()))
                .await;
        }
    })
}

fn spawn_owner_watcher(dbus: JsonDbusClient, output: OutputHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        let message = match dbus.watch_replacement().await {
            Ok(()) => "clip-daemon restarted; reconnecting".to_owned(),
            Err(error) => error.to_string(),
        };
        let _ = output.send(OutputCommand::TransportError(message)).await;
    })
}

async fn wait_for_request_slot(calls: &mut JoinSet<()>) {
    while calls.len() >= MAX_IN_FLIGHT_REQUESTS {
        let _ = calls.join_next().await;
    }
}

fn reap_finished(calls: &mut JoinSet<()>) {
    while calls.try_join_next().is_some() {}
}

async fn drain_calls(calls: &mut JoinSet<()>) {
    while calls.join_next().await.is_some() {}
}

async fn cancel_active(dbus: Option<&JsonDbusClient>, output: &OutputHandle) {
    let Some(dbus) = dbus else { return };
    for id in output.active_ids().await {
        let _ = dbus.cancel_json(&id).await;
    }
}
