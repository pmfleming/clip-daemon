use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
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
    while let Some(line) = lines.next_line().await.context("read client request")? {
        let Some(request) = decode_request(&line, &output).await? else {
            continue;
        };
        if !handle_request(request, &connection, &output).await? {
            break;
        }
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
) -> Result<bool> {
    match request {
        Request::Call { id, method, params } => {
            tokio::spawn(run_call(
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
        Request::Shutdown { id } => {
            emit(
                output,
                &json!({"kind":"response","id":id,"ok":true,"response":{"shutdown":true}}),
            )
            .await?;
            return Ok(false);
        }
    }
    Ok(true)
}

async fn run_call(
    connection: Option<zbus::Connection>,
    output: Output,
    id: String,
    method: String,
    params: Value,
) {
    let response = call(&connection, &method, params)
        .await
        .unwrap_or_else(|_| {
            api::error(
                "daemon-unavailable",
                "clip-daemon session service is unavailable".into(),
            )
        });
    let _ = emit(
        &output,
        &json!({"kind":"response","id":id,"ok":true,"response":response}),
    )
    .await;
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
