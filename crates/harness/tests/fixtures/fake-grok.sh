#!/bin/sh
# Fake Grok Build ACP agent for comet-harness tests.
#
# Speaks scripted ACP v1 JSON-RPC 2.0 over stdio: initialize handshake,
# session new/load, then a scenario picked from the session/prompt text.
# Driven by crates/harness/tests/grok.rs.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

case "$*" in
  "agent --always-approve --no-leader stdio") ;;
  "agent --always-approve --no-leader --model grok-4.5 --reasoning-effort low stdio") ;;
  "agent --always-approve --no-leader --reasoning-effort low stdio") ;;
  *) exit 1 ;;
esac
[ "$GROK_SANDBOX" = "off" ] || exit 1
# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"protocolVersion":1' || exit 1
has "$line" '"name":"comet-native"' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true,\"promptCapabilities\":{\"image\":false,\"audio\":false,\"embeddedContext\":true}},\"authMethods\":[],\"_meta\":{\"modelState\":{\"currentModelId\":\"grok-4.5\",\"availableModels\":[{\"modelId\":\"grok-4.5\",\"name\":\"Grok 4.5\",\"description\":\"Frontier\",\"_meta\":{\"supportsReasoningEffort\":true,\"reasoningEfforts\":[{\"id\":\"low\",\"value\":\"low\"},{\"id\":\"medium\",\"value\":\"medium\"},{\"id\":\"high\",\"value\":\"high\",\"default\":true}]}}]}}}}"

read -r line || exit 1 # notifications/initialized
has "$line" '"method":"notifications/initialized"' || exit 1

# Discovery-only clients close after initialize; if stdin ends, exit cleanly.
# ---- session new / load ----------------------------------------------------
read -r line || exit 0
session_line="$line"
if has "$line" '"method":"session/load"'; then
  if has "$line" '"sessionId":"resume-fail"'; then
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32603,\"message\":\"Path not found.\"}}"
    read -r line || exit 1
    has "$line" '"method":"session/new"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"sess-fresh\"}}"
  else
    emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"sess-resumed\"}}"
  fi
elif has "$line" '"method":"session/new"'; then
  emit "{\"id\":$(rid "$line"),\"result\":{\"sessionId\":\"sess-1\"}}"
else
  exit 1
fi

# ---- prompts (may be multiple for steering) --------------------------------
while read -r promptline; do
  pid=$(rid "$promptline")

  if has "$promptline" '"method":"session/cancel"'; then
    # Notification — no response; exit so the harness sees EOF.
    exit 0
  fi

  has "$promptline" '"method":"session/prompt"' || {
    # Tolerate stray notifications or other methods.
    continue
  }

  case "$promptline" in

  *scenario:happy*)
    has "$promptline" 'scenario:happy' || {
      emit "{\"id\":$pid,\"error\":{\"code\":-32600,\"message\":\"missing user prompt\"}}"
      continue
    }
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}'
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}}}'
    # tool_call open
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"list_dir","rawInput":{"target_directory":"/tmp"},"_meta":{"x.ai/tool":{"name":"list_dir","kind":"list"}}}}}'
    # enrichment — must not re-open ToolCall
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","title":"List `/tmp`","rawInput":{"target_directory":"/tmp"},"_meta":{"x.ai/tool":{"name":"list_dir","input":{"directory":"/tmp"}}}}}}'
    # completed
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","rawOutput":{"ok":true}}}}'
    # exec tool that fails
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"call-2","title":"run_terminal_command","rawInput":{"command":"false"},"_meta":{"x.ai/tool":{"name":"run_terminal_command","input":{"command":"false"}}}}}}'
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-2","status":"failed"}}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"sessionId\":\"sess-1\",\"usage\":{\"inputTokens\":42,\"outputTokens\":7,\"totalTokens\":49}}}}"
    ;;

  *scenario:steer*)
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}}}'
    # Keep the prompt active while Comet sends the ACP interjection extension.
    read -r steerline || exit 1
    sid=$(rid "$steerline")
    if has "$steerline" '"method":"_x.ai/interject"' && has "$steerline" 'redirect please' && has "$steerline" '"sessionId":"sess-1"'; then
      emit "{\"id\":$sid,\"result\":{\"result\":{\"status\":\"queued\"}}}"
      emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"steered"}}}}'
      emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"usage\":{\"inputTokens\":2,\"outputTokens\":2}}}}"
    else
      emit "{\"id\":$sid,\"error\":{\"code\":-32600,\"message\":\"expected _x.ai/interject\"}}"
    fi
    ;;

  *scenario:question*)
    emit '{"jsonrpc":"2.0","id":900,"method":"_x.ai/ask_user_question","params":{"sessionId":"sess-1","toolCallId":"call-question","questions":[{"question":"Choose one?","options":[{"label":"a","description":"first"},{"label":"b","description":"second"}],"multiSelect":false}],"mode":"default"}}'
    read -r answerline || exit 1
    if has "$answerline" '"id":900' && has "$answerline" '"outcome":"accepted"' && has "$answerline" '"Choose one?":["b"]'; then
      emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answered"}}}}'
      emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"usage\":{\"inputTokens\":2,\"outputTokens\":1}}}}"
    else
      emit "{\"id\":$pid,\"error\":{\"code\":-32600,\"message\":\"invalid question response\"}}"
    fi
    ;;

  *scenario:interrupt*)
    emit '{"method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial"}}}}'
    # Stay open until cancel notification, then exit without answering prompt.
    while read -r line; do
      if has "$line" '"method":"session/cancel"'; then
        exit 0
      fi
    done
    exit 0
    ;;

  *scenario:resume-ok*)
    emit '{"method":"session/update","params":{"sessionId":"sess-resumed","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"resumed"}}}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"usage\":{\"inputTokens\":3,\"outputTokens\":1}}}}"
    ;;

  *)
    emit '{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}'
    emit "{\"id\":$pid,\"result\":{\"stopReason\":\"end_turn\",\"_meta\":{\"usage\":{\"inputTokens\":1,\"outputTokens\":1}}}}"
    ;;
  esac
done
