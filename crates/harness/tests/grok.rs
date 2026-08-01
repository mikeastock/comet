//! GrokBuildHarness integration tests against the fake ACP agent in
//! `tests/fixtures/fake-grok.sh` (no real `grok` binary involved).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, GrokBuildHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, ToolCall,
    UserInputAnswer,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-grok.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> GrokBuildHarness {
    GrokBuildHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(50), Duration::from_millis(50))
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("grok-4.5".into()),
        reasoning: Some(ReasoningLevel::Low),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_questions| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::<UserInputAnswer>::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
    };
    (controls, steer_tx, token)
}

/// Run until the stream ends. Drops the steer sender so a completed turn
/// tears down the persistent session (same shape as production idle-reap).
async fn run_to_end(
    harness: &GrokBuildHarness,
    req: RunRequest,
    controls: RunControls,
    steer_tx: mpsc::Sender<SteerMessage>,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    // Close the mailbox so the session exits after the in-flight turn.
    drop(steer_tx);
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

#[tokio::test]
async fn happy_path_maps_deltas_tools_usage_and_done() {
    let (controls, steer_tx, _token) = controls();
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    let events = run_to_end(&harness(), req, controls, steer_tx).await;

    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                session_id,
                ..
            } => Some((harness, model, cwd, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    let (h, model, cwd, session_id) = starts[0];
    assert_eq!(*h, HarnessId::GrokBuild);
    assert_eq!(model, "grok-4.5");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "sess-1");

    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));

    // list_dir opens once despite enrichment update.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "call-1"))
            .count(),
        1,
        "tool enrichment must not re-emit ToolCall: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "call-1".into(),
        call: ToolCall::Glob {
            pattern: "/tmp".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "call-1".into(),
        is_error: false
    }));

    assert!(events.contains(&AgentEvent::ToolCall {
        id: "call-2".into(),
        call: ToolCall::Exec {
            command: "false".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "call-2".into(),
        is_error: true
    }));

    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 42,
        output_tokens: 7
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id: Some(s),
            ..
        } if s == "sess-1"
    )));
}

#[tokio::test]
async fn turn_boundary_steer_starts_follow_up_prompt() {
    let (controls, steer_tx, _token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:steer"), controls)
        .await
        .expect("run starts");
    let mut stream = std::pin::pin!(stream);

    // Wait until first Done::Completed, then steer.
    let mut saw_first_done = false;
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        let ev = ev.expect("event");
        if matches!(
            &ev,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        ) {
            saw_first_done = true;
            events.push(ev);
            let _ = steer_tx
                .send(SteerMessage {
                    prompt: "redirect please".into(),
                    message_id: None,
                })
                .await;
            // Close mailbox after steer so the session ends after the follow-up.
            drop(steer_tx);
            break;
        }
        events.push(ev);
    }
    assert!(saw_first_done, "first turn must complete: {events:?}");

    let rest: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("steered turn finished");
    events.extend(rest);

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "expected Steered: {events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "first".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    ..
                }
            ))
            .count(),
        2,
        "two completed turns: {events:?}"
    );
}

#[tokio::test]
async fn interrupt_sends_cancel_and_finishes_interrupted() {
    let (controls, _steer, token) = controls();
    let harness = harness();
    let stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");
    let mut stream = std::pin::pin!(stream);

    // Wait for partial text, then interrupt.
    while let Some(ev) = stream.next().await {
        let ev = ev.expect("event");
        if matches!(&ev, AgentEvent::TextDelta { text } if text == "partial") {
            token.cancel();
            break;
        }
    }

    let rest: Vec<_> = tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("interrupt finished");

    assert!(
        rest.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Interrupted,
                ..
            }
        )),
        "expected Interrupted: {rest:?}"
    );
}

#[tokio::test]
async fn resume_falls_back_to_session_new_on_load_failure() {
    let (controls, steer_tx, _token) = controls();
    let mut req = request("scenario:happy");
    req.resume = Some("resume-fail".into());
    let events = run_to_end(&harness(), req, controls, steer_tx).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted {
            session_id,
            ..
        } if session_id == "sess-fresh"
    )));
}

#[tokio::test]
async fn resume_load_success_uses_loaded_session() {
    let (controls, steer_tx, _token) = controls();
    let mut req = request("scenario:resume-ok");
    req.resume = Some("sess-keep".into());
    let events = run_to_end(&harness(), req, controls, steer_tx).await;
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionStarted {
            session_id,
            ..
        } if session_id == "sess-resumed"
    )));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "resumed".into()
    }));
}

#[tokio::test]
async fn models_discovers_from_initialize_model_state() {
    let models = harness().models().await.expect("models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "grok-4.5");
    assert_eq!(models[0].label, "Grok 4.5");
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Low));
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::High));
}

#[tokio::test]
async fn run_uses_initialized_current_model_when_unspecified() {
    let (controls, steer_tx, _token) = controls();
    let mut req = request("scenario:happy");
    req.model = None;
    let events = run_to_end(&harness(), req, controls, steer_tx).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::SessionStarted { model, .. } if model == "grok-4.5"
        )),
        "initialized model should populate SessionStarted: {events:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let missing = GrokBuildHarness::new().with_executable("/nonexistent/grok-nowhere");
    let err = missing.models().await.expect_err("not installed");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
    assert_eq!(missing.id(), HarnessId::GrokBuild);
    assert_eq!(missing.display_name(), "Grok Build");
    assert_eq!(
        missing.steering_mode(),
        comet_proto::SteeringMode::TurnBoundary
    );
}
