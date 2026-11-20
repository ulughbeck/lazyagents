mod capabilities;
mod commands;
mod config;
mod elicitation;
mod error;
mod events;
mod requests;
#[cfg(test)]
mod tests;
mod transport;
mod usage;

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::{timeout, timeout_at, Duration, Instant};

use crate::harness::Launch;
use capabilities::Capabilities;
pub use commands::AgentCommand;
pub use config::{ConfigKind, ConfigOption, ConfigValue};
pub use elicitation::{Choice, Elicitation, ElicitationAnswer, ElicitationOutcome, Field, Input};
pub use error::{friendly_error, PromptTimeout, ProtocolError};
pub use events::{
    CompactionEnd, Handler, Notice, PermissionRequest, Severity, StreamEvent, ToolStatus,
};
use events::{Compactions, Updates};
pub use requests::PermissionOption;
use transport::capture_stderr;
pub use usage::{ContextUsage, Usage};

const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Client {
    usage: Option<Usage>,
    context_usage: Option<ContextUsage>,
    capabilities: Capabilities,
    permissions: Option<crate::permissions::Store>,
    /// Forms and links were advertised in `initialize`.
    elicitation: bool,
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    pending_prompt: Option<u64>,
    session_id: String,
    config_options: Vec<ConfigOption>,
    commands: Vec<AgentCommand>,
    compactions: Compactions,
    /// Notices that arrived during control requests.
    notices: Vec<Notice>,
    runtime_name: &'static str,
    closed: bool,
    diagnostics: Arc<Mutex<VecDeque<String>>>,
    stderr_task: JoinHandle<()>,
}

impl Client {
    pub async fn start(
        launch: Launch,
        cwd: &Path,
        mcp_servers: Vec<Value>,
        resume_session_id: Option<&str>,
    ) -> Result<Self> {
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        for (name, value) in launch.env {
            command.env(name, value);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "could not start ACP runtime at {}",
                launch.program.display()
            )
        })?;
        let input = child.stdin.take().context("ACP adapter has no stdin")?;
        let output =
            BufReader::new(child.stdout.take().context("ACP adapter has no stdout")?).lines();
        let stderr = child.stderr.take().context("ACP adapter has no stderr")?;
        let diagnostics = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = capture_stderr(stderr, Arc::clone(&diagnostics));
        let mut client = Self {
            usage: None,
            context_usage: None,
            capabilities: Capabilities::default(),
            permissions: None,
            elicitation: launch.elicitation,
            child,
            input,
            output,
            next_id: 1,
            pending_prompt: None,
            session_id: String::new(),
            config_options: Vec::new(),
            commands: Vec::new(),
            compactions: Compactions::default(),
            notices: Vec::new(),
            runtime_name: launch.runtime_name,
            closed: false,
            diagnostics,
            stderr_task,
        };

        let mut client_capabilities = json!({"session": {
            "configOptions": {"boolean": {}}, "notices": {}, "compaction": {}
        }});
        if client.elicitation {
            client_capabilities["elicitation"] = json!({"form": {}});
        }
        let initialize = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": capabilities::PROTOCOL_VERSION,
                    "clientInfo": {"name": "lazyagents", "version": env!("CARGO_PKG_VERSION")},
                    "clientCapabilities": client_capabilities
                }),
            )
            .await
            .context("ACP initialization failed")?;
        client.capabilities = Capabilities::parse(&initialize)?;

        let mut session_params = json!({"cwd": cwd, "mcpServers": mcp_servers});
        if let Some(meta) = launch.session_meta {
            session_params["_meta"] = meta;
        }
        if let Some(text) = launch.system_prompt {
            // Never start a session without the agent instructions.
            if !client.capabilities.system_prompt() {
                bail!(
                    "the {} ACP adapter cannot receive the agent instructions; run `lazyagents repair` to update it",
                    launch.runtime_name
                );
            }
            session_params["systemPrompt"] =
                json!({"mode": "append", "content": [{"type": "text", "text": text}]});
        }
        let response = if let Some(session_id) = resume_session_id {
            session_params["sessionId"] = Value::String(session_id.to_owned());
            let method = client.capabilities.continuation_method().context(
                "the selected harness does not support resuming persistent ACP sessions",
            )?;
            let response = match client.request(method, session_params.clone()).await
            {
                Ok(response) => response,
                Err(resume_error)
                    if method == "session/resume" && client.capabilities.load_session() =>
                {
                    client
                        .request("session/load", session_params)
                        .await
                        .with_context(|| {
                            format!(
                                "could not resume ACP session {session_id:?}; session/resume failed: {resume_error:#}"
                            )
                        })?
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("could not resume ACP session {session_id:?}"));
                }
            };
            client.session_id = session_id.to_owned();
            response
        } else {
            let response = client
                .request("session/new", session_params)
                .await
                .context(
                    "could not create an ACP session; check that the selected harness is signed in",
                )?;
            client.session_id = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .context("ACP session response has no sessionId")?
                .to_owned();
            response
        };
        if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
            client.set_config_options(options);
        }
        Ok(client)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    pub fn context_usage(&self) -> Option<&ContextUsage> {
        self.context_usage.as_ref()
    }

    /// Stops an interrupted prompt and drains its response before another turn can start.
    pub async fn cancel_prompt(&mut self, handler: impl Handler) -> Result<()> {
        self.cancel_pending_prompt(&mut Updates::new(handler)).await
    }

    async fn cancel_pending_prompt<H: Handler>(&mut self, updates: &mut Updates<H>) -> Result<()> {
        let id = self
            .pending_prompt
            .context("the prompt was interrupted before it was sent; restart chat")?;
        let deadline = Instant::now() + CANCEL_DRAIN_TIMEOUT;
        timeout_at(deadline, async {
            self.write(&json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": self.session_id}
            }))
            .await?;
            self.drain_cancelled(id, updates).await
        })
        .await
        .context("the agent did not stop within five seconds; restart chat")?
    }

    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        let session_id = self.session_id.clone();
        if !session_id.is_empty() && self.capabilities.close_session() {
            let _ = timeout(
                Duration::from_secs(2),
                self.request("session/close", json!({"sessionId": session_id})),
            )
            .await;
        }
        let _ = self.input.shutdown().await;
        self.terminate_process_group(libc::SIGTERM);
        if timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            self.terminate_process_group(libc::SIGKILL);
            let _ = self.child.wait().await;
        }
        self.closed = true;
        self.stderr_task.abort();
    }

    /// Notices that arrived while no turn was active, oldest first.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    pub fn diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Sends a prompt and returns the complete assistant answer.
    pub async fn prompt(&mut self, text: &str, handler: impl Handler) -> Result<String> {
        self.prompt_with_timeout(text, handler, None).await
    }

    pub async fn prompt_with_timeout(
        &mut self,
        text: &str,
        handler: impl Handler,
        inactivity_timeout: Option<Duration>,
    ) -> Result<String> {
        let mut updates = Updates::new(handler);
        let session_id = self.session_id.clone();
        let id = self
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{"type": "text", "text": text}]
                }),
            )
            .await?;
        self.pending_prompt = Some(id);
        self.await_response(id, &mut updates, inactivity_timeout)
            .await?;
        updates.finish()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stderr_task.abort();
        if !self.closed {
            self.terminate_process_group(libc::SIGKILL);
            let _ = self.child.start_kill();
        }
    }
}
