use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use super::events::SessionUpdate;
use super::{
    Client, Handler, Notice, PromptTimeout, ProtocolError, StreamEvent, Updates, Usage,
    CONTROL_REQUEST_TIMEOUT,
};

/// One JSON-RPC message from the agent.
pub(super) enum Message {
    Request {
        id: Value,
        method: String,
        /// Kept as text, so that request handlers can read fields in their order.
        params: Box<RawValue>,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response(Response),
}

/// An agent message that arrived while no request was in flight.
pub struct Incoming(Message);

pub(super) struct Response {
    /// Only numeric ids are matched, because this client sends numeric ids.
    id: Option<u64>,
    result: Result<Value, ProtocolError>,
}

impl Message {
    /// Returns `None` for valid JSON that is not a JSON-RPC message.
    fn parse(line: &str) -> Result<Option<Self>> {
        #[derive(Deserialize)]
        struct Raw {
            id: Option<Value>,
            method: Option<String>,
            params: Option<Box<RawValue>>,
            result: Option<Value>,
            error: Option<RawError>,
        }
        #[derive(Deserialize)]
        struct RawError {
            #[serde(default)]
            code: i64,
            message: Option<String>,
        }

        let raw: Raw = match serde_json::from_str(line) {
            Ok(raw) => raw,
            Err(error) if error.is_data() => return Ok(None),
            Err(error) => return Err(error).context("ACP adapter returned invalid JSON"),
        };
        Ok(Some(match (raw.method, raw.id) {
            (Some(method), Some(id)) => Self::Request {
                id,
                method,
                params: raw.params.unwrap_or_else(|| RawValue::NULL.to_owned()),
            },
            (Some(method), None) => Self::Notification {
                method,
                params: match raw.params {
                    Some(params) => serde_json::from_str(params.get())?,
                    None => Value::Null,
                },
            },
            (None, id) => Self::Response(Response {
                id: id.as_ref().and_then(Value::as_u64),
                result: match raw.error {
                    Some(error) => Err(ProtocolError {
                        code: error.code,
                        message: error
                            .message
                            .unwrap_or_else(|| "unknown ACP error".to_owned()),
                    }),
                    None => Ok(raw.result.unwrap_or_default()),
                },
            }),
        }))
    }
}

impl Client {
    /// Sends a control request. Permission requests during it are cancelled,
    /// and notices are kept for `take_notices`.
    pub(super) async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut notices = Notices::default();
        let mut updates = Updates::new(&mut notices);
        let id = self.send_request(method, params).await?;
        let result = timeout(
            CONTROL_REQUEST_TIMEOUT,
            self.await_response(id, &mut updates, None),
        )
        .await;
        self.notices.append(&mut notices.0);
        match result {
            Ok(result) => result,
            Err(_) => bail!("ACP {method} did not respond after 30 seconds"),
        }
    }

    /// Waits for the next agent message while no request is in flight.
    /// Cancellation safe: no message is lost when another event wins a `select!`.
    pub async fn next_message(&mut self) -> Result<Incoming> {
        self.read().await.map(Incoming)
    }

    /// Handles a message from `next_message`. No turn is active, so requests
    /// are answered at once and permission requests are cancelled.
    pub async fn handle_idle(&mut self, message: Incoming, handler: impl Handler) -> Result<()> {
        self.dispatch(message.0, &mut Updates::new(handler), true)
            .await
            .map(drop)
    }

    pub(super) async fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;
        Ok(id)
    }

    /// Handles agent messages until the response to `id` arrives.
    pub(super) async fn await_response<H: Handler>(
        &mut self,
        id: u64,
        updates: &mut Updates<H>,
        inactivity_timeout: Option<Duration>,
    ) -> Result<Value> {
        loop {
            let message = if let Some(duration) = inactivity_timeout {
                match timeout(duration, self.read()).await {
                    Ok(message) => message?,
                    Err(_) => {
                        self.cancel_pending_prompt(updates).await.map_err(|error| {
                            error.context(PromptTimeout {
                                connection_ready: false,
                            })
                        })?;
                        return Err(PromptTimeout {
                            connection_ready: true,
                        }
                        .into());
                    }
                }
            } else {
                self.read().await?
            };
            let Some(response) = self.dispatch(message, updates, false).await? else {
                continue;
            };
            if response.id != Some(id) {
                continue;
            }
            if self.pending_prompt == Some(id) {
                self.pending_prompt = None;
            }
            return Ok(response.result?);
        }
    }

    /// Stops an unfinished prompt and drains messages until its response arrives.
    pub(super) async fn drain_cancelled<H: Handler>(
        &mut self,
        id: u64,
        updates: &mut Updates<H>,
    ) -> Result<()> {
        loop {
            let message = self.read().await?;
            if let Some(response) = self.dispatch(message, updates, true).await? {
                if response.id == Some(id) {
                    self.pending_prompt = None;
                    return Ok(());
                }
            }
        }
    }

    /// Answers requests and applies notifications. Returns responses to the caller.
    async fn dispatch<H: Handler>(
        &mut self,
        message: Message,
        updates: &mut Updates<H>,
        cancelled: bool,
    ) -> Result<Option<Response>> {
        match message {
            Message::Request { id, method, params } => {
                self.handle_request(id, &method, &params, updates, cancelled)
                    .await?;
            }
            Message::Notification { method, params } if method == "session/update" => {
                // A malformed update is skipped so that it cannot stop the turn.
                let Some(Ok(update)) = params.get("update").map(SessionUpdate::deserialize) else {
                    return Ok(None);
                };
                match update {
                    SessionUpdate::UsageUpdate(usage) => self.context_usage = Some(usage),
                    SessionUpdate::ConfigOptionUpdate { config_options } => {
                        self.set_config_options(&config_options);
                    }
                    SessionUpdate::AvailableCommandsUpdate { available_commands } => {
                        self.set_commands(&available_commands);
                    }
                    SessionUpdate::CompactionUpdate(update) => {
                        if let Some(end) = self.compactions.apply(update) {
                            updates.event(StreamEvent::Compaction(end))?;
                        }
                    }
                    update => updates.update(update)?,
                }
            }
            Message::Notification { .. } => {}
            Message::Response(response) => {
                if let Some(Ok(usage)) = response
                    .result
                    .as_ref()
                    .ok()
                    .and_then(|result| result.get("usage"))
                    .map(Usage::deserialize)
                {
                    self.usage = Some(usage);
                }
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    async fn read(&mut self) -> Result<Message> {
        loop {
            let Some(line) = self.output.next_line().await? else {
                let status = self.child.wait().await?;
                bail!("ACP adapter stopped unexpectedly with {status}");
            };
            if let Some(message) = Message::parse(&line)? {
                return Ok(message);
            }
        }
    }

    pub(super) async fn write(&mut self, value: &Value) -> Result<()> {
        self.input
            .write_all(serde_json::to_string(value)?.as_bytes())
            .await?;
        self.input.write_all(b"\n").await?;
        self.input.flush().await?;
        Ok(())
    }

    pub(super) fn terminate_process_group(&self, signal: libc::c_int) {
        if let Some(pid) = self.child.id() {
            // The adapter is the leader of its own process group.
            unsafe {
                libc::kill(-(pid as libc::pid_t), signal);
            }
        }
    }
}

/// Keeps notices and ignores other events.
#[derive(Default)]
struct Notices(Vec<Notice>);

impl Handler for Notices {
    fn event(&mut self, _: StreamEvent) -> Result<()> {
        Ok(())
    }

    fn notice(&mut self, notice: Notice) -> Result<()> {
        self.0.push(notice);
        Ok(())
    }
}

pub(super) fn capture_stderr(
    stderr: tokio::process::ChildStderr,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut buffer) = diagnostics.lock() {
                if buffer.len() == 50 {
                    buffer.pop_front();
                }
                buffer.push_back(line);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages_and_skips_non_rpc_json() {
        assert!(matches!(
            Message::parse(r#"{"id":"a","method":"m"}"#).unwrap(),
            Some(Message::Request { id, .. }) if id == "a"
        ));
        assert!(matches!(
            Message::parse(r#"{"method":"session/update","params":{}}"#).unwrap(),
            Some(Message::Notification { .. })
        ));
        assert!(matches!(
            Message::parse(r#"{"id":3,"error":{"code":-32603,"message":"boom"}}"#).unwrap(),
            Some(Message::Response(Response { id: Some(3), result: Err(error) }))
                if error.code == -32603 && error.message == "boom"
        ));
        assert!(Message::parse("[1, 2]").unwrap().is_none());
        assert!(Message::parse("{").is_err());
    }
}
