//! Per-session application of inbound agent events.
//!
//! These functions take an [`Emitter`] + the target [`SessionHandle`] and mutate
//! that session's state, emitting the resulting [`SessionDelta`]s. They were
//! lifted out of `AgentManager` (which used `&self` only to reach `self.emit`)
//! so BOTH drivers can call the exact same logic:
//!
//! - the legacy `AgentManager::dispatch` (event applied on the manager/driver
//!   task), and
//! - the single-owner [`crate::actor::SessionActor`] (event applied on the
//!   actor's own task, interleaved in one FIFO with the turn completion so the
//!   idle/ordering race cannot occur).
//!
//! `AgentDisconnected` is intentionally NOT handled here — it fans out across
//! all of an agent's sessions, so it stays in the manager.

use agent_client_protocol::schema::v1 as acp_schema;
use atlas_acp::{AcpEvent, AgentId};
use parking_lot::Mutex;

use crate::events::{Emitter, SessionDelta, SessionDeltaEnvelope};
use crate::session::{
    MessageMode, MessageRole, PlanEntry, SessionState, SessionStatus, ToolCall, ToolCallStatus,
    extract_image_block, extract_text_block, format_tool_content, map_tool_status,
    new_assistant_text,
    new_assistant_thinking, new_assistant_tool, normalise_tool_input,
};

/// Apply one inbound [`AcpEvent`] to an already-resolved session. Handles every
/// per-session variant; `AgentDisconnected` (agent-wide) must be handled by the
/// caller.
pub fn apply_event(emitter: &Emitter, state: &Mutex<SessionState>, agent_id: AgentId, event: AcpEvent) {
    match event {
        AcpEvent::AgentDisconnected { .. } => {
            // Agent-wide — handled by the manager's dispatch fan-out.
        }
        AcpEvent::SessionUpdate {
            session_id: _,
            update,
        } => {
            apply_session_update(emitter, state, update);
        }
        AcpEvent::PermissionRequest {
            request_id,
            session_id: _,
            tool_call,
            options,
        } => {
            // The turn is paused waiting on the user (e.g. a plan / tool
            // approval). Surface a distinct `Waiting` status so the UI keeps an
            // active affordance instead of looking idle/"done".
            let (sid, turn_seq) = {
                let mut st = state.lock();
                st.status = SessionStatus::Waiting;
                (st.session_id.clone(), st.turn_seq)
            };
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid.clone(),
                delta: SessionDelta::Status {
                    status: SessionStatus::Waiting,
                    turn_seq,
                },
            });
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::PermissionRequest {
                    request_id,
                    tool_call: serde_json::to_value(&tool_call).unwrap_or_default(),
                    options: serde_json::to_value(&options).unwrap_or_default(),
                },
            });
        }
        AcpEvent::Retry {
            session_id: _,
            attempt,
            max_attempts,
            delay_ms,
            last_error,
        } => {
            let sid = state.lock().session_id.clone();
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::RetryStatus {
                    attempt,
                    max_attempts,
                    delay_ms,
                    last_error,
                },
            });
        }
        AcpEvent::Usage {
            session_id: _,
            input_tokens,
            output_tokens,
            cost,
        } => {
            let mut st = state.lock();
            st.usage.input_tokens = input_tokens;
            st.usage.output_tokens = output_tokens;
            st.usage.cost = cost;
            let usage = st.usage.clone();
            let sid = st.session_id.clone();
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::UsageUpdated { usage },
            });
        }
        AcpEvent::Compaction {
            session_id: _,
            active,
        } => {
            let sid = state.lock().session_id.clone();
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::Compaction { active },
            });
        }
        AcpEvent::CompressionSaved {
            session_id: _,
            saved_tokens,
        } => {
            let sid = state.lock().session_id.clone();
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::CompressionSaved { saved_tokens },
            });
        }
    }
}

fn apply_session_update(
    emitter: &Emitter,
    state: &Mutex<SessionState>,
    update: acp_schema::SessionUpdate,
) {
    // Decode through JSON — `SessionUpdate` is `#[non_exhaustive]` and its
    // variants carry schema types that are awkward to match directly. The wire
    // form is stable, so going through serde_json gives us a clean dispatch on
    // `sessionUpdate`.
    let Ok(v) = serde_json::to_value(&update) else {
        return;
    };
    let Some(kind) = v.get("sessionUpdate").and_then(|s| s.as_str()) else {
        return;
    };
    match kind {
        "agent_message_chunk" => {
            let Some(content) = v.get("content") else {
                return;
            };
            let Ok(block) = serde_json::from_value::<acp_schema::ContentBlock>(content.clone())
            else {
                return;
            };
            if let Some(text) = extract_text_block(&block) {
                append_text_chunk(emitter, state, text);
            } else if let Some((mime, data)) = extract_image_block(&block) {
                // Agent-sent images fold into the existing markdown pipeline
                // as a data-URI image — no new delta variant or frontend path.
                append_text_chunk(
                    emitter,
                    state,
                    format!("\n\n![image](data:{mime};base64,{data})\n\n"),
                );
            }
        }
        "agent_thought_chunk" => {
            let Some(content) = v.get("content") else {
                return;
            };
            let Ok(block) = serde_json::from_value::<acp_schema::ContentBlock>(content.clone())
            else {
                return;
            };
            if let Some(text) = extract_text_block(&block) {
                append_thinking_chunk(emitter, state, text);
            }
        }
        "tool_call" | "tool_call_update" => {
            apply_tool_call(emitter, state, &v, kind == "tool_call_update");
        }
        "plan" => {
            let entries: Vec<PlanEntry> = v
                .get("entries")
                .and_then(|e| serde_json::from_value(e.clone()).ok())
                .unwrap_or_default();
            let mut st = state.lock();
            st.plan = entries.clone();
            // Attach to the trailing assistant message, or seed a fresh one.
            let attached = if let Some(last) = st.messages.last_mut() {
                if matches!(last.role, MessageRole::Assistant) {
                    last.plan = Some(entries.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !attached {
                let mut msg = new_assistant_text(String::new());
                msg.plan = Some(entries.clone());
                msg.model = st.current_model.clone();
                st.messages.push(msg);
            }
            st.touch();
            let sid = st.session_id.clone();
            let agent_id = st.agent_id;
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::PlanUpdated { plan: entries },
            });
        }
        "available_commands_update" => {
            let commands: Vec<serde_json::Value> = v
                .get("availableCommands")
                .and_then(|c| c.as_array().cloned())
                .unwrap_or_default();
            let mut st = state.lock();
            st.available_commands = commands.clone();
            let sid = st.session_id.clone();
            let agent_id = st.agent_id;
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::AvailableCommands { commands },
            });
        }
        "current_mode_update" => {
            let Some(mode_id) = v.get("currentModeId").and_then(|s| s.as_str()) else {
                return;
            };
            let mut st = state.lock();
            st.current_mode = Some(mode_id.to_string());
            let sid = st.session_id.clone();
            let agent_id = st.agent_id;
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::ModeChanged {
                    mode_id: mode_id.to_string(),
                },
            });
        }
        "usage_update" => {
            // ACP context-window gauge (`used`/`size` tokens, optional cost).
            // Cost is a `{ amount, currency }` object per ACP schema.
            let used = v.get("used").and_then(|x| x.as_u64()).unwrap_or(0);
            let size = v.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
            let cost = v
                .get("cost")
                .and_then(|c| c.get("amount"))
                .and_then(|a| a.as_f64())
                .unwrap_or(0.0);
            if used == 0 && size == 0 {
                return;
            }
            let st = state.lock();
            let sid = st.session_id.clone();
            let agent_id = st.agent_id;
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::ContextUsage { used, size, cost },
            });
        }
        "current_model_update" => {
            let Some(model_id) = v.get("currentModelId").and_then(|s| s.as_str()) else {
                return;
            };
            let mut st = state.lock();
            st.current_model = Some(model_id.to_string());
            let sid = st.session_id.clone();
            let agent_id = st.agent_id;
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::ModelChanged {
                    model_id: model_id.to_string(),
                },
            });
        }
        _ => {}
    }
}

fn append_text_chunk(emitter: &Emitter, state: &Mutex<SessionState>, text: String) {
    let mut st = state.lock();
    let (delta, agent_id, sid) = match st.messages.last_mut() {
        Some(last)
            if last.role == MessageRole::Assistant
                && last.mode == MessageMode::Text
                && last.tool_calls.is_empty() =>
        {
            last.content.push_str(&text);
            let msg_id = last.id.clone();
            let agent_id = st.agent_id;
            let sid = st.session_id.clone();
            st.touch();
            (
                SessionDelta::TextChunk {
                    message_id: msg_id,
                    delta: text,
                },
                agent_id,
                sid,
            )
        }
        _ => {
            let mut msg = new_assistant_text(text);
            msg.model = st.current_model.clone();
            let cloned = msg.clone();
            st.messages.push(msg);
            let agent_id = st.agent_id;
            let sid = st.session_id.clone();
            st.touch();
            (SessionDelta::MessageAppended { message: cloned }, agent_id, sid)
        }
    };
    drop(st);
    emitter.emit(SessionDeltaEnvelope {
        agent_id,
        session_id: sid,
        delta,
    });
}

fn append_thinking_chunk(emitter: &Emitter, state: &Mutex<SessionState>, text: String) {
    let mut st = state.lock();
    let (delta, agent_id, sid) = match st.messages.last_mut() {
        Some(last)
            if last.role == MessageRole::Assistant && last.mode == MessageMode::Thinking =>
        {
            last.thinking.push_str(&text);
            let msg_id = last.id.clone();
            let agent_id = st.agent_id;
            let sid = st.session_id.clone();
            st.touch();
            (
                SessionDelta::ThinkingChunk {
                    message_id: msg_id,
                    delta: text,
                },
                agent_id,
                sid,
            )
        }
        _ => {
            let mut msg = new_assistant_thinking(text);
            msg.model = st.current_model.clone();
            let cloned = msg.clone();
            st.messages.push(msg);
            let agent_id = st.agent_id;
            let sid = st.session_id.clone();
            st.touch();
            (SessionDelta::MessageAppended { message: cloned }, agent_id, sid)
        }
    };
    drop(st);
    emitter.emit(SessionDeltaEnvelope {
        agent_id,
        session_id: sid,
        delta,
    });
}

fn apply_tool_call(
    emitter: &Emitter,
    state: &Mutex<SessionState>,
    v: &serde_json::Value,
    is_update: bool,
) {
    let Some(tool_call_id) = v.get("toolCallId").and_then(|s| s.as_str()) else {
        return;
    };
    let raw_input_val = v.get("rawInput");
    let title = v.get("title").and_then(|s| s.as_str()).map(|s| s.to_string());
    let kind = v.get("kind").and_then(|s| s.as_str()).map(|s| s.to_string());
    let status_raw = v.get("status").and_then(|s| s.as_str());
    let content_val = v.get("content").cloned();
    let formatted = content_val.as_ref().and_then(format_tool_content);

    let mut st = state.lock();

    // Upsert by toolCallId across existing messages.
    for msg in st.messages.iter_mut().rev() {
        if let Some(tc) = msg.tool_calls.iter_mut().find(|t| t.id == tool_call_id) {
            tc.status = map_tool_status(status_raw, tc.status);
            if let Some(input) = raw_input_val {
                tc.arguments = normalise_tool_input(Some(input));
            }
            if let Some(t) = title.clone() {
                // Only the display title is refreshed. `tool_name` keeps its
                // first-sighting value: for the native agent that IS the real
                // tool name ("Read", "Bash"), and overwriting it with a later
                // title threw that away — leaving no field anywhere carrying the
                // name, which is what session capture needs to group by.
                tc.title = Some(t);
            }
            if let Some(k) = kind.clone() {
                tc.kind = Some(k);
            }
            // Locations arrive late. Many agents attach them only on the
            // completion update, and this branch previously never read them —
            // so the paths an edit touched were silently lost, and the visible
            // symptom was a Checkpoint that never formed. Empty is treated as
            // "no news" rather than "cleared", because an update carrying only
            // a status must not erase paths we already have.
            if let Some(locations) = v.get("locations").and_then(|l| l.as_array()) {
                if !locations.is_empty() {
                    tc.locations = locations.clone();
                }
            }
            if let Some(result) = formatted.clone() {
                tc.result = Some(result);
            }
            let updated = tc.clone();
            let msg_id = msg.id.clone();
            let agent_id = st.agent_id;
            let sid = st.session_id.clone();
            st.touch();
            drop(st);
            emitter.emit(SessionDeltaEnvelope {
                agent_id,
                session_id: sid,
                delta: SessionDelta::ToolCallUpserted {
                    message_id: msg_id,
                    tool_call: updated,
                },
            });
            return;
        }
    }

    // First sighting — only the initial `tool_call` event creates a new message;
    // lone `tool_call_update` for an unknown id is ignored to match the frontend.
    if is_update {
        return;
    }

    let tool_call = ToolCall {
        id: tool_call_id.to_string(),
        tool_name: title
            .clone()
            .or_else(|| kind.clone())
            .unwrap_or_else(|| "tool".to_string()),
        title: title.clone(),
        kind: kind.clone(),
        status: map_tool_status(status_raw, ToolCallStatus::Running),
        arguments: normalise_tool_input(raw_input_val),
        result: formatted,
        locations: v
            .get("locations")
            .and_then(|l| l.as_array().cloned())
            .unwrap_or_default(),
    };
    let mut msg = new_assistant_tool(tool_call.clone());
    msg.model = st.current_model.clone();
    let msg_id = msg.id.clone();
    st.messages.push(msg);
    let agent_id = st.agent_id;
    let sid = st.session_id.clone();
    st.touch();
    drop(st);
    emitter.emit(SessionDeltaEnvelope {
        agent_id,
        session_id: sid,
        delta: SessionDelta::ToolCallUpserted {
            message_id: msg_id,
            tool_call,
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::DeltaSink;
    use std::sync::Arc;

    struct NullSink;
    impl DeltaSink for NullSink {
        fn emit(&self, _envelope: SessionDeltaEnvelope) {}
    }

    fn session() -> (Emitter, Mutex<SessionState>) {
        let emitter = Emitter::new(Arc::new(NullSink));
        let state = Mutex::new(SessionState::new(
            AgentId(uuid::Uuid::nil()),
            "sess-1".into(),
            "/tmp/project".into(),
            "claude-code".into(),
        ));
        (emitter, state)
    }

    /// The only tool call in the session.
    fn only_tool_call(state: &Mutex<SessionState>) -> ToolCall {
        let st = state.lock();
        st.messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .next()
            .expect("a tool call")
            .clone()
    }

    #[test]
    fn locations_arriving_only_on_a_completion_update_are_captured() {
        // The defect this pins: many agents attach `locations` only when the
        // call completes, and the upsert branch used to never read the field.
        // The symptom was not a visible error — it was a Checkpoint that never
        // formed, because the paths the edit touched were silently lost.
        let (emitter, state) = session();

        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "Edit src/lib.rs",
                "kind": "edit",
                "status": "in_progress",
            }))
            .expect("tool_call decodes"),
        );
        assert!(only_tool_call(&state).locations.is_empty());

        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-1",
                "status": "completed",
                "locations": [{ "path": "/tmp/project/src/lib.rs" }],
            }))
            .expect("tool_call_update decodes"),
        );

        let tc = only_tool_call(&state);
        assert_eq!(tc.status, ToolCallStatus::Completed);
        assert_eq!(tc.locations.len(), 1, "locations from the update were dropped");
        assert_eq!(tc.locations[0]["path"], "/tmp/project/src/lib.rs");
    }

    #[test]
    fn a_later_update_without_locations_does_not_erase_the_ones_we_have() {
        let (emitter, state) = session();

        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "Edit src/lib.rs",
                "kind": "edit",
                "status": "in_progress",
                "locations": [{ "path": "/tmp/project/src/lib.rs" }],
            }))
            .unwrap(),
        );
        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-1",
                "status": "completed",
            }))
            .unwrap(),
        );

        assert_eq!(only_tool_call(&state).locations.len(), 1);
    }

    #[test]
    fn the_first_sighting_tool_name_survives_a_later_title_change() {
        // For the native agent the first-sighting title IS the real tool name
        // ("Read", "Bash"). Overwriting `tool_name` with every later title threw
        // that away, leaving no field anywhere carrying the name that session
        // capture needs to group by.
        let (emitter, state) = session();

        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "Bash",
                "kind": "execute",
                "status": "in_progress",
            }))
            .unwrap(),
        );
        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-1",
                "title": "Bash(cargo test --package atlas-review)",
                "status": "completed",
            }))
            .unwrap(),
        );

        let tc = only_tool_call(&state);
        assert_eq!(tc.tool_name, "Bash", "the name must not churn with the title");
        assert_eq!(
            tc.title.as_deref(),
            Some("Bash(cargo test --package atlas-review)"),
            "the display title must still refresh"
        );
    }

    #[test]
    fn a_failed_tool_call_keeps_its_failed_status() {
        let (emitter, state) = session();
        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1",
                "title": "Bash",
                "kind": "execute",
                "status": "in_progress",
            }))
            .unwrap(),
        );
        apply_session_update(
            &emitter,
            &state,
            serde_json::from_value(serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-1",
                "status": "failed",
            }))
            .unwrap(),
        );
        assert_eq!(only_tool_call(&state).status, ToolCallStatus::Failed);
    }
}
