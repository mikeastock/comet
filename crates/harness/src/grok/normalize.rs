//! Grok Build ACP `session/update` → [`AgentEvent`] mapping.
//!
//! Tolerant by construction: tool names come from `_meta["x.ai/tool"].name`
//! when present (with `title` / raw input as fallbacks); unknown tools map to
//! [`ToolCall::Unknown`]. Tool enrichment across `tool_call` /
//! `tool_call_update` must not re-open a ToolCall lifecycle — callers track
//! seen ids and only emit [`AgentEvent::ToolResult`] on terminal status.

use comet_proto::{AgentEvent, TodoItem, ToolCall};
use serde_json::Value;

/// Extract text from an ACP content block (`{"type":"text","text":...}`) or a
/// bare string.
pub(crate) fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

/// `params.update` from a `session/update` notification.
pub(crate) fn update_payload(params: &Value) -> &Value {
    params.get("update").unwrap_or(params)
}

pub(crate) fn session_update_kind(update: &Value) -> &str {
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Map one `session/update` payload into zero or more events.
///
/// `seen_tools` tracks toolCallIds that already emitted a ToolCall so
/// enrichment updates do not duplicate. Returns events and whether each
/// newly-opened tool id should be recorded (caller updates the set).
pub(crate) fn map_session_update(
    update: &Value,
    seen_tools: &std::collections::HashSet<String>,
) -> Vec<AgentEvent> {
    match session_update_kind(update) {
        "agent_message_chunk" => content_text(update.get("content").unwrap_or(&Value::Null))
            .map(|text| AgentEvent::TextDelta { text })
            .into_iter()
            .collect(),
        "agent_thought_chunk" => content_text(update.get("content").unwrap_or(&Value::Null))
            .map(|text| AgentEvent::ReasoningDelta { text })
            .into_iter()
            .collect(),
        "tool_call" | "tool_call_update" => map_tool_update(update, seen_tools),
        // user_message_chunk, available_commands_update, plan, etc. — ignore.
        _ => Vec::new(),
    }
}

fn map_tool_update(
    update: &Value,
    seen_tools: &std::collections::HashSet<String>,
) -> Vec<AgentEvent> {
    let id = update
        .get("toolCallId")
        .or_else(|| update.get("tool_call_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if id.is_empty() {
        return Vec::new();
    }

    let status = update
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut events = Vec::new();

    // Open the lifecycle once — either on the initial tool_call or on the
    // first update that arrives before we saw the open event.
    if !seen_tools.contains(&id) {
        events.push(AgentEvent::ToolCall {
            id: id.clone(),
            call: map_tool_call(update),
        });
    }

    match status.as_str() {
        "completed" | "success" | "ok" => {
            events.push(AgentEvent::ToolResult {
                id,
                is_error: false,
            });
        }
        "failed" | "error" | "cancelled" | "canceled" => {
            events.push(AgentEvent::ToolResult { id, is_error: true });
        }
        // pending / in_progress / empty: lifecycle open only (or no-op if seen).
        _ => {}
    }

    events
}

/// Decode a tool invocation from an ACP tool_call / tool_call_update.
pub(crate) fn map_tool_call(update: &Value) -> ToolCall {
    let (name, input) = tool_name_and_input(update);
    map_named_tool(&name, input.as_ref())
}

fn tool_name_and_input(update: &Value) -> (String, Option<Value>) {
    let meta_tool = update
        .get("_meta")
        .and_then(|m| m.get("x.ai/tool"))
        .filter(|t| t.is_object());

    let name = meta_tool
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .or_else(|| update.get("title").and_then(Value::as_str))
        .or_else(|| update.get("kind").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_owned();

    // Prefer the canonical x.ai/tool.input when present; fall back to rawInput.
    let input = meta_tool
        .and_then(|t| t.get("input"))
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| {
            update
                .get("rawInput")
                .or_else(|| update.get("raw_input"))
                .filter(|v| !v.is_null())
                .cloned()
        });

    (name, input)
}

fn map_named_tool(name: &str, input: Option<&Value>) -> ToolCall {
    let input = input.unwrap_or(&Value::Null);
    match name {
        "run_terminal_command" => ToolCall::Exec {
            command: str_field(
                input,
                &["command", "cmd", "shell_command"],
                input.as_str().unwrap_or(""),
            ),
        },
        "read_file" => ToolCall::ReadFile {
            path: str_field(input, &["path", "target_file", "file", "filename"], ""),
        },
        "search_replace" => ToolCall::EditFile {
            path: str_field(input, &["path", "file_path", "file", "target_file"], ""),
            old_string: opt_str_field(input, &["old_string", "oldString", "old"]),
            new_string: opt_str_field(input, &["new_string", "newString", "new"]),
        },
        "grep" => ToolCall::Search {
            pattern: str_field(input, &["pattern", "query", "regex"], ""),
            path: opt_str_field(input, &["path", "directory", "target_directory"]),
        },
        "list_dir" => ToolCall::Glob {
            // Comet has no ListDir kind; Glob renders a path/pattern label.
            pattern: str_field(
                input,
                &["directory", "target_directory", "path", "pattern"],
                "",
            ),
        },
        "web_search" => ToolCall::WebSearch {
            query: str_field(input, &["query", "q", "search"], ""),
        },
        "web_fetch" => ToolCall::WebFetch {
            url: str_field(input, &["url", "uri", "href"], ""),
            prompt: opt_str_field(input, &["prompt", "instructions", "query"]),
        },
        "todo_write" => ToolCall::Todo {
            items: todo_items(input),
        },
        "use_tool" => map_use_tool(input),
        other => ToolCall::Unknown {
            name: other.to_owned(),
            input: if input.is_null() {
                None
            } else {
                Some(input.clone())
            },
        },
    }
}

/// `use_tool` → Mcp when server/tool can be extracted; else Unknown.
fn map_use_tool(input: &Value) -> ToolCall {
    let server = opt_str_field(
        input,
        &["server", "server_name", "mcp_server", "serverName"],
    );
    let tool = opt_str_field(input, &["tool", "tool_name", "name", "toolName"]);
    match (server, tool) {
        (Some(server), Some(tool)) => ToolCall::Mcp {
            server,
            tool,
            input: Some(input.clone()),
        },
        _ => ToolCall::Unknown {
            name: "use_tool".into(),
            input: if input.is_null() {
                None
            } else {
                Some(input.clone())
            },
        },
    }
}

fn todo_items(input: &Value) -> Vec<TodoItem> {
    // Accept `todos` / `items` arrays.
    let arr = input
        .get("todos")
        .or_else(|| input.get("items"))
        .and_then(Value::as_array);
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .map(|t| TodoItem {
            text: str_field(t, &["text", "content", "title", "subject"], ""),
            done: todo_done(t),
        })
        .collect()
}

fn todo_done(t: &Value) -> bool {
    if let Some(b) = t
        .get("done")
        .or_else(|| t.get("completed"))
        .and_then(Value::as_bool)
    {
        return b;
    }
    t.get("status")
        .and_then(Value::as_str)
        .is_some_and(|st| matches!(st, "done" | "completed" | "complete" | "finished"))
}

fn str_field(v: &Value, keys: &[&str], default: &str) -> String {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_owned()
}

fn opt_str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Usage from a `session/prompt` result's `_meta.usage` (or top-level token
/// fields as a fallback).
pub(crate) fn usage_from_prompt_result(result: &Value) -> Option<AgentEvent> {
    let meta = result.get("_meta");
    let usage = meta
        .and_then(|m| m.get("usage"))
        .filter(|u| u.is_object())
        .or(meta);
    let usage = usage?;
    let input = usage
        .get("inputTokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            result
                .get("_meta")
                .and_then(|m| m.get("inputTokens"))
                .and_then(Value::as_u64)
        })?;
    let output = usage
        .get("outputTokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| {
            result
                .get("_meta")
                .and_then(|m| m.get("outputTokens"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    Some(AgentEvent::Usage {
        input_tokens: input,
        output_tokens: output,
    })
}

/// Whether a prompt `stopReason` should surface as an error Done.
pub(crate) fn stop_reason_error(result: &Value) -> Option<String> {
    let reason = result
        .get("stopReason")
        .or_else(|| result.get("stop_reason"))
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    match reason {
        "end_turn" | "end-turn" | "stop" | "completed" | "cancelled" | "canceled"
        | "interrupted" => None,
        "max_tokens" | "max-tokens" => Some("Grok stopped: max tokens".into()),
        "refusal" => Some("Grok refused the request".into()),
        other => {
            // Unknown stop reasons are not hard errors — the turn still ended.
            tracing::debug!(target: "comet_harness::grok", "unhandled stopReason: {other}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn message_and_thought_chunks() {
        let text = map_session_update(
            &json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Hello"}
            }),
            &HashSet::new(),
        );
        assert_eq!(
            text,
            vec![AgentEvent::TextDelta {
                text: "Hello".into()
            }]
        );

        let thought = map_session_update(
            &json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "thinking"}
            }),
            &HashSet::new(),
        );
        assert_eq!(
            thought,
            vec![AgentEvent::ReasoningDelta {
                text: "thinking".into()
            }]
        );
    }

    #[test]
    fn tool_call_opens_once_and_closes_on_completed() {
        let mut seen = HashSet::new();
        let open = map_session_update(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "list_dir",
                "rawInput": {"target_directory": "/tmp"},
                "_meta": {
                    "x.ai/tool": {
                        "name": "list_dir",
                        "kind": "list"
                    }
                }
            }),
            &seen,
        );
        assert_eq!(
            open,
            vec![AgentEvent::ToolCall {
                id: "call-1".into(),
                call: ToolCall::Glob {
                    pattern: "/tmp".into()
                },
            }]
        );
        seen.insert("call-1".into());

        // Enrichment must not re-emit ToolCall.
        let enrich = map_session_update(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "title": "List `/tmp`",
                "rawInput": {"target_directory": "/tmp"},
                "_meta": {
                    "x.ai/tool": {
                        "name": "list_dir",
                        "input": {"directory": "/tmp"}
                    }
                }
            }),
            &seen,
        );
        assert!(enrich.is_empty(), "{enrich:?}");

        let done = map_session_update(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "rawOutput": {"ok": true}
            }),
            &seen,
        );
        assert_eq!(
            done,
            vec![AgentEvent::ToolResult {
                id: "call-1".into(),
                is_error: false
            }]
        );
    }

    #[test]
    fn typed_tool_mapping() {
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "run_terminal_command",
                    "input": {"command": "ls -la"}}},
                "rawInput": {"command": "ignored"}
            })),
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "title": "read_file",
                "rawInput": {"target_file": "src/main.rs"}
            })),
            ToolCall::ReadFile {
                path: "src/main.rs".into()
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "search_replace"}},
                "rawInput": {
                    "path": "a.rs",
                    "old_string": "old",
                    "new_string": "new"
                }
            })),
            ToolCall::EditFile {
                path: "a.rs".into(),
                old_string: Some("old".into()),
                new_string: Some("new".into()),
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "grep"}},
                "rawInput": {"pattern": "foo", "path": "src"}
            })),
            ToolCall::Search {
                pattern: "foo".into(),
                path: Some("src".into())
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "web_search"}},
                "rawInput": {"query": "rust"}
            })),
            ToolCall::WebSearch {
                query: "rust".into()
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "web_fetch"}},
                "rawInput": {"url": "https://example.com"}
            })),
            ToolCall::WebFetch {
                url: "https://example.com".into(),
                prompt: None
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "todo_write"}},
                "rawInput": {"todos": [
                    {"text": "a", "done": true},
                    {"text": "b", "completed": false}
                ]}
            })),
            ToolCall::Todo {
                items: vec![
                    TodoItem {
                        text: "a".into(),
                        done: true
                    },
                    TodoItem {
                        text: "b".into(),
                        done: false
                    },
                ]
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "use_tool"}},
                "rawInput": {"server": "linear", "tool": "search", "q": "bug"}
            })),
            ToolCall::Mcp {
                server: "linear".into(),
                tool: "search".into(),
                input: Some(json!({"server": "linear", "tool": "search", "q": "bug"})),
            }
        );
        assert_eq!(
            map_tool_call(&json!({
                "_meta": {"x.ai/tool": {"name": "spawn_subagent"}},
                "rawInput": {"prompt": "go"}
            })),
            ToolCall::Unknown {
                name: "spawn_subagent".into(),
                input: Some(json!({"prompt": "go"})),
            }
        );
    }

    #[test]
    fn usage_from_prompt_meta() {
        let result = json!({
            "stopReason": "end_turn",
            "_meta": {
                "inputTokens": 100,
                "outputTokens": 20,
                "usage": {
                    "inputTokens": 17719,
                    "outputTokens": 25,
                    "totalTokens": 17744
                }
            }
        });
        assert_eq!(
            usage_from_prompt_result(&result),
            Some(AgentEvent::Usage {
                input_tokens: 17719,
                output_tokens: 25
            })
        );
        assert_eq!(stop_reason_error(&result), None);
    }

    #[test]
    fn failed_tool_status() {
        let mut seen = HashSet::new();
        seen.insert("c1".into());
        let ev = map_session_update(
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "c1",
                "status": "failed"
            }),
            &seen,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true
            }]
        );
    }
}
