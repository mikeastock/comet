//! Grok Build harness: spawns the installed `grok` CLI as
//! `grok agent --always-approve --no-leader […flags] stdio` with
//! `GROK_SANDBOX=off` and speaks ACP v1 (JSON-RPC 2.0, newline-delimited) over
//! stdio.
//!
//! - `initialize` (protocolVersion 1, empty clientCapabilities, Comet
//!   clientInfo) then `notifications/initialized`.
//! - `session/new` (or `session/load` with a fresh-start fallback on resume) →
//!   [`AgentEvent::SessionStarted`]; `session/prompt` carries the user text as
//!   ACP content blocks. Grok owns and executes its tools.
//! - `session/update` notifications map to text/thought deltas and typed
//!   tool calls; prompt-result `_meta.usage` becomes Usage before Done.
//! - Steering uses Grok's `_x.ai/interject` ACP extension while a prompt is
//!   active. A steer that arrives between prompts starts another
//!   `session/prompt` on the same session.
//! - Interrupt: `session/cancel` notification, then SIGTERM → SIGKILL; stream
//!   ends with `Done { status: Interrupted }`.
//! - Auth is Grok's own (`~/.grok/auth.json` / env). No Comet account UI.
//! - Grok's `_x.ai/ask_user_question` reverse request is bridged through
//!   [`RunControls::request_input`] and answered on the ACP connection.

mod catalog;
mod normalize;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls};
use catalog::{REASONING_LEVELS, models_from_model_state, to_effort};
use normalize::{map_session_update, stop_reason_error, update_payload, usage_from_prompt_result};

/// Locate the device's installed Grok CLI: `GROK_EXECUTABLE`, then PATH, then
/// common install locations GUI launches miss. Resolved per call — cheap, and
/// PATH may be adopted from the login shell after startup.
fn resolve_grok_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GROK_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "grok.exe" } else { "grok" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".grok").join("bin").join("grok"));
        candidates.push(home.join("bin").join("grok"));
        candidates.push(home.join(".local").join("bin").join("grok"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/grok"));
    candidates.push(PathBuf::from("/usr/local/bin/grok"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// The Grok Build harness. Construct with [`GrokBuildHarness::new`]; tests
/// point it at a fake CLI with [`GrokBuildHarness::with_executable`].
pub struct GrokBuildHarness {
    executable: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
}

impl Default for GrokBuildHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl GrokBuildHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the cancel→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_grok_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "grok (searched PATH, ~/.grok/bin, ~/bin, ~/.local/bin, \
                 /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun install \
                 dirs; set GROK_EXECUTABLE to override)"
                    .into(),
            )
        })
    }

    /// Build the child `Command` for `grok agent … stdio`.
    fn agent_command(
        &self,
        exe: &std::path::Path,
        request: &RunRequest,
        for_discovery: bool,
    ) -> Command {
        let mut cmd = Command::new(exe);
        cmd.arg("agent");
        cmd.arg("--always-approve");
        cmd.arg("--no-leader");
        if !for_discovery {
            if let Some(model) = &request.model
                && !model.is_empty()
            {
                cmd.arg("--model").arg(model);
            }
            if let Some(effort) = to_effort(request.reasoning) {
                cmd.arg("--reasoning-effort").arg(effort);
            }
        }
        cmd.arg("stdio");
        crate::compose_child_path(&mut cmd, exe);
        // Grok runs as an unattended local harness: approvals are handled by
        // --always-approve and the process must never inherit a restrictive
        // sandbox from either the Comet request or the parent environment.
        cmd.env("GROK_SANDBOX", "off");
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

#[async_trait]
impl Harness for GrokBuildHarness {
    fn id(&self) -> HarnessId {
        HarnessId::GrokBuild
    }
    fn display_name(&self) -> &str {
        "Grok Build"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Grok's interjection extension injects at the next safe point in a live turn.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }

    /// Live model discovery via a short-lived ACP initialize handshake.
    /// Fails as [`HarnessError::NotInstalled`] when the binary is missing and
    /// surfaces protocol failures honestly — no hard-coded stale catalog.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = self.agent_command(
            &exe,
            &RunRequest {
                prompt: String::new(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: String::new(),
                sandbox: comet_proto::SandboxLevel::ReadOnly,
                auto_approve: true,
                resume: None,
                attachments: Vec::new(),
            },
            true,
        );
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("grok child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("grok child has no stdout".into()))?;
        // Drain stderr so a full pipe can't wedge the child during discovery.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::grok", "discovery stderr: {line}");
                }
            });
        }

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let discover = async {
            let result = client
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": 1,
                        "clientCapabilities": {},
                        "clientInfo": {
                            "name": "comet-native",
                            "title": "Comet",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                )
                .await?;
            client.notify("notifications/initialized", None);
            let model_state = result
                .get("_meta")
                .and_then(|m| m.get("modelState"))
                .cloned()
                .unwrap_or(Value::Null);
            if model_state.is_null() {
                return Err(HarnessError::Protocol(
                    "initialize: missing _meta.modelState".into(),
                ));
            }
            let (_current, models) = models_from_model_state(&model_state);
            if models.is_empty() {
                return Err(HarnessError::Protocol(
                    "initialize: modelState has no models".into(),
                ));
            }
            Ok(models)
        };

        let outcome = tokio::time::timeout(Duration::from_secs(15), discover).await;
        // Close the transport so the discovery-only child can exit cleanly.
        drop(client);
        drop(incoming);
        shutdown_child(&mut child, Duration::from_secs(2)).await;

        match outcome {
            Ok(Ok(models)) => Ok(models),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HarnessError::Protocol(
                "model discovery timed out waiting for initialize".into(),
            )),
        }
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let mut cmd = self.agent_command(&exe, &request, false);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("grok child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("grok child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::grok", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// ACP content blocks for a user prompt.
fn prompt_content(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = std::sync::Arc::new(request_input);

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        let initialize = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "comet-native",
                        "title": "Comet",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        client.notify("notifications/initialized", None);

        let current_model = initialize
            .get("_meta")
            .and_then(|meta| meta.get("modelState"))
            .and_then(|state| state.get("currentModelId"))
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);

        let cwd = if request.cwd.is_empty() {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".into())
        } else {
            request.cwd.clone()
        };

        let session_id = if let Some(resume) = &request.resume {
            let load_params = json!({
                "sessionId": resume,
                "cwd": cwd,
                "mcpServers": [],
            });
            match client.request("session/load", load_params).await {
                Ok(result) => result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or(resume)
                    .to_owned(),
                Err(e) => {
                    tracing::debug!(
                        target: "comet_harness::grok",
                        "session/load failed (starting fresh): {e}"
                    );
                    let result = client
                        .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                        .await?;
                    result
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                }
            }
        } else {
            let result = client
                .request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
                .await?;
            result
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };

        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "session/new: missing sessionId".into(),
            ));
        }
        Ok::<(String, String, Option<String>), HarnessError>((session_id, cwd, current_model))
    };

    let (session_id, cwd, current_model) = tokio::select! {
        res = setup => match res {
            Ok(v) => v,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let mut assistant_message_id = new_message_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::GrokBuild,
            model: request.model.clone().or(current_model).unwrap_or_default(),
            tools: Vec::new(),
            cwd,
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // ---- prompt loop state ------------------------------------------------
    let mut seen_tools: HashSet<String> = HashSet::new();
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut prompt_active = true;
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // In-flight prompt request: spawned so the select loop keeps draining
    // session/update notifications while the RPC awaits its response.
    let mut pending_prompt: Option<tokio::task::JoinHandle<Result<Value, HarnessError>>> = {
        let client = client.clone();
        let params = json!({
            "sessionId": session_id,
            "prompt": prompt_content(&request.prompt),
        });
        Some(tokio::spawn(async move {
            client.request("session/prompt", params).await
        }))
    };

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    if method == "session/update" {
                        let update = update_payload(&params);
                        let events = map_session_update(update, &seen_tools);
                        for ev in events {
                            if let AgentEvent::ToolCall { id, .. } = &ev {
                                seen_tools.insert(id.clone());
                            }
                            if !send(&event_tx, ev).await {
                                break 'main;
                            }
                        }
                    }
                    // Other notifications (_x.ai/*, etc.) are tolerated.
                }

                Some(Incoming::Request { id, method, params }) => {
                    if method == "_x.ai/ask_user_question" {
                        handle_user_question(
                            id,
                            params,
                            &request_input,
                            &client,
                        );
                    } else {
                        tracing::debug!(
                            target: "comet_harness::grok",
                            "unhandled server request: {method}"
                        );
                        client.respond_error(
                            &id,
                            -32601,
                            &format!("unsupported method: {method}"),
                        );
                    }
                }

                Some(Incoming::Eof) | None => break 'main,
            },

            prompt_result = async {
                match pending_prompt.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending().await,
                }
            }, if pending_prompt.is_some() => {
                pending_prompt = None;
                prompt_active = false;
                match prompt_result {
                    Ok(Ok(result)) => {
                        if let Some(usage) = usage_from_prompt_result(&result)
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        let error = stop_reason_error(&result);
                        let status = if interrupted {
                            DoneStatus::Interrupted
                        } else if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        // Close the assistant message part before Done.
                        let (prev, _next) = rotate(&mut assistant_message_id);
                        if !send(
                            &event_tx,
                            AgentEvent::AssistantMessageCompleted {
                                assistant_message_id: prev,
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        done_current = true;
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(session_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                            break 'main;
                        }
                        // Deliver a buffered steer as the next turn, or wait.
                        if let Some(text) = queued_steers.pop_front() {
                            if !start_steered_prompt(
                                &client,
                                &session_id,
                                &text,
                                &mut assistant_message_id,
                                &event_tx,
                                &mut pending_prompt,
                                &mut prompt_active,
                                &mut done_current,
                                &mut seen_tools,
                            )
                            .await
                            {
                                break 'main;
                            }
                        } else if !steering_open {
                            break 'main;
                        }
                    }
                    Ok(Err(e)) => {
                        if interrupted {
                            done_after_interrupt = true;
                            let _ = send(
                                &event_tx,
                                AgentEvent::Done {
                                    status: DoneStatus::Interrupted,
                                    result: None,
                                    error: None,
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await;
                        } else {
                            let _ = send(
                                &event_tx,
                                AgentEvent::Done {
                                    status: DoneStatus::Errored,
                                    result: None,
                                    error: Some(e.to_string()),
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await;
                        }
                        break 'main;
                    }
                    Err(join_err) => {
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Errored,
                                result: None,
                                error: Some(format!("prompt task failed: {join_err}")),
                                session_id: Some(session_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }
                }
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    if prompt_active {
                        let params = json!({
                            "sessionId": session_id,
                            "text": text,
                            "interjectionId": uuid::Uuid::new_v4().to_string(),
                        });
                        match client.request("_x.ai/interject", params).await {
                            Ok(_) => {
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::Steered {
                                        assistant_message_id: Some(prev),
                                        next_assistant_message_id: Some(next),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                            Err(e) => {
                                // Preserve the user's message if an older Grok
                                // binary rejects the extension or the active turn
                                // wins the completion race.
                                tracing::debug!(
                                    target: "comet_harness::grok",
                                    "interject rejected (queued as next turn): {e}"
                                );
                                queued_steers.push_back(text);
                            }
                        }
                    } else if !start_steered_prompt(
                        &client,
                        &session_id,
                        &text,
                        &mut assistant_message_id,
                        &event_tx,
                        &mut pending_prompt,
                        &mut prompt_active,
                        &mut done_current,
                        &mut seen_tools,
                    )
                    .await
                    {
                        break 'main;
                    }
                }
                None => {
                    steering_open = false;
                    if !prompt_active && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                // session/cancel is a notification (no response).
                client.notify(
                    "session/cancel",
                    Some(json!({ "sessionId": session_id })),
                );
                if prompt_active {
                    if let Some(pid) = child.id() {
                        let grace = interrupt_grace;
                        let kill = kill_grace;
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns — nothing in flight.
                    break 'main;
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("grok agent", status, &stderr_tail)),
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    if let Some(handle) = pending_prompt {
        handle.abort();
    }
    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Emit Steered, rotate the assistant message id, and start a follow-up
/// `session/prompt` on the same session. Returns false when the consumer hung up.
#[allow(clippy::too_many_arguments)]
async fn start_steered_prompt(
    client: &RpcClient,
    session_id: &str,
    text: &str,
    assistant_message_id: &mut String,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    pending_prompt: &mut Option<tokio::task::JoinHandle<Result<Value, HarnessError>>>,
    prompt_active: &mut bool,
    done_current: &mut bool,
    seen_tools: &mut HashSet<String>,
) -> bool {
    let (prev, next) = rotate(assistant_message_id);
    if !send(
        event_tx,
        AgentEvent::Steered {
            assistant_message_id: Some(prev),
            next_assistant_message_id: Some(next),
        },
    )
    .await
    {
        return false;
    }
    *done_current = false;
    seen_tools.clear();
    let client = client.clone();
    let params = json!({
        "sessionId": session_id,
        "prompt": prompt_content(text),
    });
    *pending_prompt = Some(tokio::spawn(async move {
        client.request("session/prompt", params).await
    }));
    *prompt_active = true;
    true
}

/// Serve Grok's blocking question extension without stalling the ACP receive
/// loop. The engine owns the visible InputRequested/InputResolved lifecycle;
/// this adapter only translates between Grok's question wire shape and the
/// normalized Comet question bridge.
fn handle_user_question(
    id: Value,
    params: Value,
    request_input: &std::sync::Arc<RequestInputFn>,
    client: &RpcClient,
) {
    let questions = parse_user_questions(&params);
    if questions.is_empty() {
        client.respond_error(&id, -32602, "ask_user_question contained no questions");
        return;
    }

    let request_input = std::sync::Arc::clone(request_input);
    let client = client.clone();
    tokio::spawn(async move {
        let answers = (request_input)(questions.clone()).await.unwrap_or_default();
        client.respond(&id, grok_question_response(&questions, &answers));
    });
}

fn parse_user_questions(params: &Value) -> Vec<UserInputQuestion> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| questions.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|question| UserInputQuestion {
            id: uuid::Uuid::new_v4().to_string(),
            header: "Question".into(),
            question: question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            options: question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| options.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|option| option.get("label").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            multi_select: question
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect()
}

/// Grok keys answers by question text. Free-form Comet answers use Grok's
/// `Other` + annotation representation so typed text is not mistaken for an
/// option label.
fn grok_question_response(questions: &[UserInputQuestion], answers: &[UserInputAnswer]) -> Value {
    let mut by_question = serde_json::Map::new();
    let mut annotations = serde_json::Map::new();

    for question in questions {
        let Some(answer) = answers
            .iter()
            .find(|answer| answer.question_id == question.id)
        else {
            continue;
        };
        if answer.labels.is_empty() {
            continue;
        }

        let mut selected = Vec::new();
        let mut notes = Vec::new();
        for label in &answer.labels {
            if question.options.contains(label) {
                selected.push(Value::String(label.clone()));
            } else {
                notes.push(label.clone());
            }
        }
        if !notes.is_empty() {
            selected.push(Value::String("Other".into()));
            annotations.insert(
                question.question.clone(),
                json!({ "notes": notes.join(", ") }),
            );
        }
        by_question.insert(question.question.clone(), Value::Array(selected));
    }

    if by_question.is_empty() {
        json!({ "outcome": "cancelled" })
    } else if annotations.is_empty() {
        json!({ "outcome": "accepted", "answers": by_question })
    } else {
        json!({
            "outcome": "accepted",
            "answers": by_question,
            "annotations": annotations,
        })
    }
}

// ---------------------------------------------------------------------------
// Child lifecycle
// ---------------------------------------------------------------------------

async fn shutdown_child(child: &mut Child, kill_grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> UserInputQuestion {
        UserInputQuestion {
            id: "q1".into(),
            header: "Question".into(),
            question: "Choose one?".into(),
            options: vec!["a".into(), "b".into()],
            multi_select: false,
        }
    }

    #[test]
    fn grok_question_response_preserves_selected_labels() {
        let response = grok_question_response(
            &[question()],
            &[UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["b".into()],
            }],
        );
        assert_eq!(
            response,
            json!({
                "outcome": "accepted",
                "answers": { "Choose one?": ["b"] },
            })
        );
    }

    #[test]
    fn grok_question_response_uses_annotations_for_freeform() {
        let response = grok_question_response(
            &[question()],
            &[UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["something else".into()],
            }],
        );
        assert_eq!(
            response,
            json!({
                "outcome": "accepted",
                "answers": { "Choose one?": ["Other"] },
                "annotations": { "Choose one?": { "notes": "something else" } },
            })
        );
    }

    #[test]
    fn grok_question_response_cancels_when_no_answers_arrive() {
        assert_eq!(
            grok_question_response(&[question()], &[]),
            json!({ "outcome": "cancelled" })
        );
    }
}
