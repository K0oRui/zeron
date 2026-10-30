//! ACP `session/update` → [`AgentEvent`] mapping (protocol v1, the line the
//! claude/codex adapters and Grok Build speak today).
//!
//! Tolerant by construction, like the codex normalizer: decoded from raw
//! [`Value`]s, unknown `sessionUpdate` kinds and content types map to nothing,
//! and missing fields degrade to empty strings rather than errors. Wire shapes
//! verified against `agent-client-protocol-schema` 1.3.0 (`SessionUpdate` is
//! tagged `sessionUpdate`/snake_case; structs are camelCase; tool kinds and
//! statuses are snake_case).

use serde_json::Value;
use zeron_proto::{AgentEvent, SlashCommand, TodoItem, TodoStatus, ToolCall, ToolDiff};

/// Byte cap applied to tool output text at the harness boundary. The doc-side
/// fold applies its own (smaller) cap before anything persists; this one only
/// bounds what crosses the event stream.
pub(crate) const OUTPUT_CAP: usize = 16 * 1024;

/// Byte cap for each side of an inline diff crossing the event stream.
pub(crate) const DIFF_TEXT_CAP: usize = 64 * 1024;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// Truncate on a char boundary, marking the cut so the UI can say "truncated".
pub(crate) fn cap_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push_str("\n… [truncated]");
    out
}

/// The text of a `ContentBlock` (`{type: "text", text}`); non-text blocks
/// (image, audio, resource, resource_link) render as nothing.
fn content_block_text(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// A `ContentChunk`'s streamed text (`{content: {type: "text", ...}}`).
pub(crate) fn chunk_text(update: &Value) -> Option<String> {
    let text = content_block_text(update.get("content")?)?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// Joined text of a tool call's `content` array, capped; `None` when empty.
fn tool_output(update: &Value) -> Option<String> {
    let parts: Vec<&str> = update
        .get("content")?
        .as_array()?
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("content"))
        .filter_map(|c| content_block_text(c.get("content")?))
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(cap_text(&parts.join("\n"), OUTPUT_CAP))
}

/// First `{type: "diff"}` entry of a tool call's `content` array.
fn tool_diff(update: &Value) -> Option<ToolDiff> {
    let diff = update
        .get("content")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("diff"))?;
    let path = str_field(diff, "path");
    if path.is_empty() {
        return None;
    }
    Some(ToolDiff {
        path,
        old_text: diff
            .get("oldText")
            .and_then(Value::as_str)
            .map(|t| cap_text(t, DIFF_TEXT_CAP)),
        new_text: cap_text(
            diff.get("newText").and_then(Value::as_str).unwrap_or(""),
            DIFF_TEXT_CAP,
        ),
    })
}

/// The grok-native tool name stamped on a tool call's `_meta` (`x.ai/tool`,
/// present on every grok tool_call — verified live, 1.0.4).
pub(crate) fn xai_tool_name(update: &Value) -> Option<&str> {
    update.get("_meta")?.get("x.ai/tool")?.get("name")?.as_str()
}

/// First location path (`locations: [{path, line?}]`), for read/edit calls.
fn first_location(update: &Value) -> Option<String> {
    let path = update.get("locations")?.as_array()?.first()?.get("path")?;
    path.as_str().filter(|p| !p.is_empty()).map(str::to_owned)
}

/// Cursor (and similar ACP agents) put a human summary in `title` — generic
/// labels ("Read File", "grep") before args arrive, or a markdown-wrapped
/// command (`` `ls -la` `` with inner backticks escaped as `\``). Those are
/// display strings, not typed arguments; using them as path/pattern/command
/// dumps the label (and its escapes) into the transcript chip.
fn is_placeholder_title(title: &str) -> bool {
    matches!(
        title.trim(),
        "grep"
            | "Find"
            | "Terminal"
            | "Read File"
            | "Edit File"
            | "Delete File"
            | "Web Search"
            | "Web Fetch"
            | "Codebase Search"
            | "Read TODOs"
            | "Update TODOs"
            | "Read Lints"
            | "Task: Subagent task"
            | "Subagent task"
            | "List MCP Resources"
            | "Fetch MCP Resource"
    )
}

/// Unwrap a single markdown code span used as an ACP exec title.
fn unwrap_command_title(title: &str) -> Option<String> {
    let inner = title.trim().strip_prefix('`')?.strip_suffix('`')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'`') {
            chars.next();
            out.push('`');
        } else {
            out.push(c);
        }
    }
    let out = out.trim();
    if out.is_empty() || is_placeholder_title(out) {
        None
    } else {
        Some(out.to_owned())
    }
}

fn arg_from_title(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() || is_placeholder_title(t) {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Reduce an ACP tool call (kind + title + rawInput + locations + diff
/// content) to the typed [`ToolCall`] zeron renders. Best-effort: agents vary
/// in how much structure they put in `rawInput`, so every arm has a fallback.
/// Title is only used when it looks like a real arg — never a placeholder
/// label or markdown-escaped summary.
fn typed_call(update: &Value) -> ToolCall {
    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        // Grok's OPENING `tool_call` frame carries no top-level `kind` — it
        // stamps it on `_meta.x.ai/tool` instead (`{name, kind, label,
        // read_only}`), and only the first `tool_call_update` repeats it.
        // Without this fallback the chip is born
        // `Unknown { name: "run_terminal_command" }` (snake_case, no Exec
        // icon) and re-types a beat later.
        .or_else(|| {
            update
                .get("_meta")?
                .get("x.ai/tool")?
                .get("kind")?
                .as_str()
        })
        .unwrap_or("other");
    let title = str_field(update, "title");
    let raw = update.get("rawInput").filter(|v| !v.is_null());
    let raw_str = |key: &str| -> Option<String> {
        raw.and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match kind {
        "execute" => ToolCall::Exec {
            command: raw_str("command")
                .or_else(|| unwrap_command_title(&title))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "read" => ToolCall::ReadFile {
            path: raw_str("path")
                .or_else(|| raw_str("file_path"))
                .or_else(|| raw_str("filePath"))
                .or_else(|| first_location(update))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "edit" | "delete" | "move" => {
            // A diff pins down the file and shape; otherwise fall back to the
            // location/rawInput path with unknown content.
            if let Some(diff) = tool_diff(update) {
                if diff.old_text.is_none() {
                    ToolCall::WriteFile {
                        path: diff.path,
                        content: None,
                    }
                } else {
                    ToolCall::EditFile {
                        path: diff.path.clone(),
                        old_string: diff.old_text.clone(),
                        new_string: Some(diff.new_text.clone()),
                    }
                }
            } else {
                match raw_str("path")
                    .or_else(|| raw_str("file_path"))
                    .or_else(|| raw_str("filePath"))
                    .or_else(|| first_location(update))
                {
                    Some(path) if kind == "edit" => ToolCall::EditFile {
                        path,
                        old_string: None,
                        new_string: None,
                    },
                    path => ToolCall::ApplyPatch { path },
                }
            }
        }
        "search" => {
            // Cursor web search is kind "search" + rawInput.searchTerm; grep
            // / glob / codebase search use pattern or query.
            if let Some(query) = raw_str("searchTerm") {
                ToolCall::WebSearch { query }
            } else {
                ToolCall::Search {
                    pattern: raw_str("pattern")
                        .or_else(|| raw_str("globPattern"))
                        .or_else(|| raw_str("query"))
                        .or_else(|| arg_from_title(&title))
                        .unwrap_or_default(),
                    path: raw_str("path"),
                }
            }
        }
        "fetch" => match raw_str("url") {
            Some(url) => ToolCall::WebFetch { url, prompt: None },
            None => ToolCall::WebSearch {
                query: raw_str("searchTerm")
                    .or_else(|| raw_str("query"))
                    .or_else(|| arg_from_title(&title))
                    .unwrap_or_default(),
            },
        },
        // Kindless (or unknown-kind) update carrying a diff: an edit in all
        // but name — some agents only attach the diff on the completion
        // update without repeating the kind.
        _ if tool_diff(update).is_some() => {
            let diff = tool_diff(update).expect("guarded");
            if diff.old_text.is_none() {
                ToolCall::WriteFile {
                    path: diff.path,
                    content: None,
                }
            } else {
                ToolCall::EditFile {
                    path: diff.path.clone(),
                    old_string: diff.old_text.clone(),
                    new_string: Some(diff.new_text.clone()),
                }
            }
        }
        // Grok's subagent spawn: name the chip — and the subagent tab it
        // opens — after the task, matching the claude driver's "Agent: {d}"
        // (the bare tool name says nothing in a tab strip).
        _ if xai_tool_name(update) == Some("spawn_subagent") => ToolCall::Unknown {
            name: raw_str("description")
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: raw.cloned(),
        },
        // Devin's run_subagent tool uses a coarse ACP kind; its private meta
        // field is the stable identity across pending/in-progress frames.
        _ if update
            .get("_meta")
            .and_then(|m| m.get("cognition.ai/inferenceToolName"))
            .and_then(Value::as_str)
            == Some("run_subagent") =>
        {
            ToolCall::Unknown {
                name: raw_str("title")
                    .map(|d| format!("Agent: {d}"))
                    .unwrap_or_else(|| "Agent".into()),
                input: raw.cloned(),
            }
        }
        // opencode's subagent spawn (`task` tool — rawInput carries
        // description/prompt/subagent_type): same naming as grok's, so the
        // chip and its subagent tab say what the agent is doing.
        _ if raw.is_some_and(|r| r.get("subagent_type").is_some() && r.get("prompt").is_some()) => {
            ToolCall::Unknown {
                name: raw_str("description")
                    .map(|d| format!("Agent: {d}"))
                    .unwrap_or_else(|| "Agent".into()),
                input: raw.cloned(),
            }
        }
        // Its completion drops rawInput but keeps a title (the description),
        // which would re-type the chip to a bare Unknown and cost it the
        // Agent icon/label. The rawOutput metadata (child + parent session
        // ids) still marks the spawn — keep the naming.
        _ if update
            .get("rawOutput")
            .and_then(|r| r.get("metadata"))
            .is_some_and(|m| {
                m.get("sessionId").is_some() && m.get("parentSessionId").is_some()
            }) =>
        {
            ToolCall::Unknown {
                name: if title.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {title}")
                },
                input: raw.cloned(),
            }
        }
        _ if raw_str("_toolName").as_deref() == Some("task") => ToolCall::Unknown {
            name: raw_str("description")
                .filter(|d| d != "Subagent task")
                .map(|d| format!("Task: {d}"))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_else(|| {
                    if title.is_empty() {
                        "Task".into()
                    } else {
                        title
                    }
                }),
            input: raw.cloned(),
        },
        _ => ToolCall::Unknown {
            name: if title.is_empty() { kind.into() } else { title },
            input: raw.cloned(),
        },
    }
}

/// Map one `session/update` payload's `update` object to events.
/// Message/thought chunks are handled here too (unlike codex, ACP has no
/// separate delta channel).
pub(crate) fn map_update(update: &Value) -> Vec<AgentEvent> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::TextDelta { text }])
            .unwrap_or_default(),
        "agent_thought_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::ReasoningDelta { text }])
            .unwrap_or_default(),
        // Replayed history on session/load is filtered by the session loop
        // before this map; a live user chunk is our own prompt echoed back.
        "user_message_chunk" => Vec::new(),
        "tool_call" => {
            let id = str_field(update, "toolCallId");
            let mut events = vec![AgentEvent::ToolCall {
                id: id.clone(),
                call: typed_call(update),
            }];
            // Some agents send a single terminal-status `tool_call` with the
            // result inline instead of a follow-up update.
            if let Some(resolved) = resolved_result(update, id) {
                events.push(resolved);
            }
            events
        }
        "tool_call_update" => {
            let id = str_field(update, "toolCallId");
            let mut events = Vec::new();
            // Refresh the call only when the update carries new SHAPE — kind,
            // title, rawInput, or a diff. Result-only content (output text)
            // must not re-type the call: a kindless completion update would
            // clobber the opening call's `Exec` into `Unknown`.
            if update.get("kind").is_some()
                || update.get("title").is_some()
                || update.get("rawInput").is_some()
                || tool_diff(update).is_some()
            {
                events.push(AgentEvent::ToolCall {
                    id: id.clone(),
                    call: typed_call(update),
                });
            }
            if let Some(resolved) = resolved_result(update, id) {
                events.push(resolved);
            }
            events
        }
        "plan" => {
            let items = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    TodoItem::new(
                        str_field(e, "content"),
                        TodoStatus::parse(e.get("status").and_then(Value::as_str).unwrap_or("")),
                    )
                })
                .collect();
            // The plan has no wire id; a stable synthetic id makes every
            // update refresh the same chip (fold refreshes in place by id).
            vec![
                AgentEvent::ToolCall {
                    id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
                    call: ToolCall::Todo { items },
                },
                AgentEvent::ToolResult {
                    id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                },
            ]
        }
        "available_commands_update" => {
            let commands = parse_commands(update.get("availableCommands"));
            vec![AgentEvent::AvailableCommands { commands }]
        }
        "usage_update" => {
            let tokens = update.get("used").and_then(Value::as_u64);
            let window = ["max", "limit", "size", "contextWindow", "context_window"]
                .iter()
                .find_map(|key| update.get(*key).and_then(Value::as_u64))
                .filter(|n| *n > 0);
            if tokens.is_some() || window.is_some() {
                vec![AgentEvent::ContextUsage { tokens, window }]
            } else {
                Vec::new()
            }
        }
        "current_mode_update" | "config_option_update" | "session_info_update" => Vec::new(),
        _ => notice_events(update),
    }
}

/// Grok's `retry_state` payload differs per variant (1.0.70, captured live
/// against a failing gateway):
///
/// - `retrying`:  `{error_type, is_rate_limited, attempt, max_retries, reason}`
/// - `failed`:    `{error_type, message}` — NO `attempt`, and `message` is a
///   multi-line diagnostic ("Unauthorized (401) from …" plus an indented
///   `Model:`/`Auth:`/`Version:` block), so only its first line is a sentence
/// - `exhausted`: `{attempts, is_rate_limited, reason}` — `attempts`, PLURAL,
///   and no `error_type` at all
///
/// `error_type` is a category (`api`, `http`, `auth`, `rate_limited`), never
/// prose: rendering it raw is what produced "Grok could not reach the model:
/// api". It is only consulted when the frame carries no text, and then as a
/// phrase ([`retry_category`]).
const RETRY_TEXT_CAP: usize = 200;

/// The frame's own sentence, whichever field this variant uses.
fn retry_reason(update: &Value) -> Option<String> {
    ["reason", "message"]
        .iter()
        .filter_map(|key| update.get(*key).and_then(Value::as_str))
        .map(|text| text.lines().next().unwrap_or("").trim())
        .find(|line| !line.is_empty())
        .map(|line| cap_text(line, RETRY_TEXT_CAP))
}

/// A human phrase for the machine category, for frames that carry no text.
fn retry_category(update: &Value) -> Option<&'static str> {
    match str_field(update, "error_type").as_str() {
        "api" => Some("the model API failed"),
        "http" => Some("the connection to the API failed"),
        "auth" => Some("the sign-in was rejected"),
        "rate_limited" | "rate_limit" => Some("the account is rate limited"),
        "stream" => Some("the model stream broke"),
        "timeout" => Some("the request timed out"),
        _ => None,
    }
}

/// How many attempts the frame reports: `attempt` on `retrying`, `attempts`
/// on `exhausted`, and nothing at all on `failed`.
fn retry_attempts(update: &Value) -> u64 {
    ["attempt", "attempts"]
        .iter()
        .find_map(|key| update.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

fn retry_max_retries(update: &Value) -> u64 {
    update
        .get("max_retries")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

/// What to tell the user about a retry that ended the turn: `failed` (the
/// agent gave up at once) or `exhausted` (it burned its retry budget). The
/// run loop folds this into the terminal error, so "The agent failed to
/// complete the turn." can say WHY.
pub(crate) fn retry_failure(update: &Value) -> Option<String> {
    let kind = str_field(update, "type");
    if !matches!(kind.as_str(), "failed" | "exhausted") {
        return None;
    }
    // Nothing to add means nothing to add: the Done already says the turn
    // failed, and inventing text for an unrecognized category is worse than
    // the generic sentence.
    let why = retry_reason(update).or_else(|| retry_category(update).map(str::to_owned))?;
    let attempts = retry_attempts(update);
    Some(if attempts > 1 {
        format!("{why} (after {attempts} attempts)")
    } else {
        why
    })
}

/// Updates that tell the user something the transcript would otherwise hide.
/// Grok (1.0.70) is the source: `retry_state` (the model stream died and the
/// agent is silently retrying — the run reads as frozen for minutes) and
/// context-compaction trouble, which arrives either on `session/update` or
/// wrapped in `_x.ai/session_notification`. `map_update`'s catch-all and
/// [`crate::acp::session_update_events`] both route here, so either channel
/// surfaces them.
///
/// The chips go out as [`AgentEvent::Error`] — zeron's mid-run notice
/// channel (the doc folds it into a warning chip); it does not end the turn.
pub(crate) fn notice_events(update: &Value) -> Vec<AgentEvent> {
    let message = |text: String| vec![AgentEvent::Error { message: text }];
    match str_field(update, "sessionUpdate").as_str() {
        "retry_state" => {
            match str_field(update, "type").as_str() {
                // One chip per streak: attempt 1 is where a streak starts, and
                // every attempt would bury the transcript (1.0.70 retries up
                // to 15 times).
                "retrying" if retry_attempts(update) <= 1 => {
                    let why = retry_reason(update)
                        .or_else(|| retry_category(update).map(str::to_owned));
                    let max = retry_max_retries(update);
                    message(match (why, max) {
                        (Some(why), max) if max > 1 => {
                            format!("Grok is retrying: {why} (up to {max} attempts)")
                        }
                        (Some(why), _) => format!("Grok is retrying: {why}"),
                        (None, max) if max > 1 => {
                            format!("Grok is retrying a failed model call (up to {max} attempts)")
                        }
                        (None, _) => "Grok is retrying a failed model call".into(),
                    })
                }
                // The give-up states are the END of the turn, and the run
                // loop folds them into its Done error ([`retry_failure`]).
                // The doc writes an error part for a `Done` error too, so a
                // chip here would say everything twice (the convention codex
                // states in `normalize::settle_turn_error`).
                _ => Vec::new(),
            }
        }
        "auto_compact_failed" => message(
            "Grok could not compact the conversation; the next turn may exceed its context window"
                .into(),
        ),
        "auto_compact_cancelled" => message(
            "Grok cancelled context compaction; the next turn may exceed its context window".into(),
        ),
        _ => Vec::new(),
    }
}

/// A terminal `status` on a tool_call/tool_call_update resolves the call:
/// `completed`/`failed` → ToolResult with capped output + inline diff.
fn resolved_result(update: &Value, id: String) -> Option<AgentEvent> {
    let status = update.get("status").and_then(Value::as_str)?;
    let is_error = match status {
        "completed" => false,
        "failed" => true,
        _ => return None,
    };
    Some(AgentEvent::ToolResult {
        id,
        is_error,
        output: tool_output(update),
        diff: tool_diff(update),
    })
}

/// Decode an `availableCommands` array (`{name, description, input: {hint}}`).
pub(crate) fn parse_commands(value: Option<&Value>) -> Vec<SlashCommand> {
    value
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let name = str_field(c, "name");
            (!name.is_empty()).then(|| SlashCommand {
                name,
                description: str_field(c, "description"),
                input_hint: c
                    .get("input")
                    .and_then(|i| i.get("hint"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// `session/request_permission` options (`{optionId, name, kind}`) → the
/// preferred auto-approve choice: `allow_always` > `allow_once` > first.
pub(crate) fn preferred_allow_option(options: &[Value]) -> Option<String> {
    let by_kind = |kind: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
    };
    by_kind("allow_always")
        .or_else(|| by_kind("allow_once"))
        .or_else(|| options.first())
        .map(|o| str_field(o, "optionId"))
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_and_thought_chunks_map_to_deltas() {
        let update = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::TextDelta {
                text: "hello".into()
            }]
        );
        let thought = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" },
        });
        assert_eq!(
            map_update(&thought),
            vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]
        );
        // Non-text blocks render as nothing.
        let image = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "image", "data": "...", "mimeType": "image/png" },
        });
        assert_eq!(map_update(&image), Vec::new());
    }

    #[test]
    fn execute_tool_call_with_terminal_status_resolves_inline() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "ls -la",
            "kind": "execute",
            "status": "completed",
            "rawInput": { "command": "ls -la" },
            "content": [
                { "type": "content", "content": { "type": "text", "text": "total 0" } },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::Exec {
                        command: "ls -la".into()
                    },
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false,
                    output: Some("total 0".into()),
                    diff: None,
                },
            ]
        );
    }

    /// Grok's opening tool_call frame has no top-level `kind` — it is only on
    /// `_meta.x.ai/tool.kind` — and the chip must be typed from the first
    /// frame, not after the follow-up update.
    #[test]
    fn grok_tool_call_typing_falls_back_to_the_xai_meta_kind() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "run_terminal_command",
            "rawInput": { "command": "echo hello", "description": "say hello" },
            "_meta": {
                "x.ai/tool": {
                    "name": "run_terminal_command",
                    "kind": "execute",
                    "label": "Run Command",
                    "namespace": "grok_build",
                    "read_only": false,
                },
            },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "call_1".into(),
                call: ToolCall::Exec {
                    command: "echo hello".into(),
                },
            }]
        );
        // A top-level kind still wins over the meta one.
        let explicit = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_2",
            "kind": "read",
            "rawInput": { "path": "/w/a.rs" },
            "_meta": { "x.ai/tool": { "kind": "execute" } },
        });
        assert!(matches!(
            map_update(&explicit).first(),
            Some(AgentEvent::ToolCall {
                call: ToolCall::ReadFile { .. },
                ..
            })
        ));
    }

    /// A silent retry streak is the main "Grok looks stuck" report: one chip
    /// when it starts, none for attempts 2..n. Payloads captured live from
    /// 1.0.70 against a failing gateway.
    #[test]
    fn grok_retry_states_surface_one_chip_per_streak() {
        let retrying = |attempt: u64| {
            json!({
                "sessionUpdate": "retry_state",
                "type": "retrying",
                "attempt": attempt,
                "max_retries": 15,
                "reason": "API error (status 500 Internal Server Error): mock upstream 500",
                "error_type": "api",
            })
        };
        assert_eq!(
            notice_events(&retrying(1)),
            vec![AgentEvent::Error {
                message: "Grok is retrying: API error (status 500 Internal Server Error): \
                          mock upstream 500 (up to 15 attempts)"
                    .into()
            }]
        );
        assert!(notice_events(&retrying(2)).is_empty());
        // The give-up frames are the turn's END, carried by the Done instead
        // of a chip: the doc already writes an error part for a Done error.
        assert!(
            notice_events(&json!({
                "sessionUpdate": "retry_state",
                "type": "exhausted",
                "attempts": 4,
                "reason": "API error (status 429 Too Many Requests): rate_limit_error",
                "is_rate_limited": true,
            }))
            .is_empty()
        );
        // A frame with no text at all still names the category as a phrase.
        assert_eq!(
            notice_events(&json!({
                "sessionUpdate": "retry_state",
                "type": "retrying",
                "attempt": 1,
                "max_retries": 0,
                "error_type": "http",
            })),
            vec![AgentEvent::Error {
                message: "Grok is retrying: the connection to the API failed".into()
            }]
        );
        // Lifecycle noise stays silent.
        assert!(notice_events(&json!({ "sessionUpdate": "turn_completed" })).is_empty());
        assert!(
            notice_events(&json!({
                "sessionUpdate": "auto_compact_failed",
            }))
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message } if message.contains("compact")))
        );
    }

    /// The `failed` variant spells its text `message`, not `reason`, and that
    /// message is a multi-line diagnostic. Only its first line is a sentence,
    /// and the machine `error_type` must never reach the user — that is the
    /// "Grok could not reach the model: api" report.
    #[test]
    fn grok_failed_retry_text_never_leaks_the_error_type() {
        let failed = json!({
            "sessionUpdate": "retry_state",
            "type": "failed",
            "error_type": "auth",
            "message": "Unauthorized (401) from http://127.0.0.1:8905/v1/chat/completions: \
                        authentication_error: mock unauthorized\n\n  Model:     grok-4.6\n  \
                        Auth:      ApiKey\n  Version:   1.0.70\n  Available: (none)",
        });
        // No chip: the run loop's Done carries it.
        assert!(notice_events(&failed).is_empty());
        let why = retry_failure(&failed).expect("failed is a give-up state");
        assert!(
            why.starts_with("Unauthorized (401) from http://127.0.0.1:8905/v1/chat/completions"),
            "{why}"
        );
        assert!(!why.contains("Model:"), "the diagnostic block is dropped: {why}");
        // A frame with only the machine category must not echo it.
        let why = retry_failure(&json!({
            "sessionUpdate": "retry_state",
            "type": "failed",
            "error_type": "api",
        }))
        .expect("failed is a give-up state");
        assert_eq!(why, "the model API failed");
        assert!(!why.contains("api"), "the raw error_type leaked: {why}");
        // An unknown category is never echoed raw, and adds nothing: the
        // generic Done sentence stands rather than inventing text.
        assert_eq!(
            retry_failure(&json!({
                "sessionUpdate": "retry_state",
                "type": "failed",
                "error_type": "some_future_kind",
            })),
            None
        );
    }

    /// What the run loop folds into the terminal error: the give-up states
    /// answer, the still-retrying one does not.
    #[test]
    fn grok_retry_failure_describes_only_the_give_up_states() {
        assert_eq!(
            retry_failure(&json!({
                "sessionUpdate": "retry_state",
                "type": "failed",
                "error_type": "api",
                "message": "stream ended before a finish reason\n\n  Model: grok-4.6",
            })),
            Some("stream ended before a finish reason".to_owned())
        );
        assert_eq!(
            retry_failure(&json!({
                "sessionUpdate": "retry_state",
                "type": "exhausted",
                "attempts": 3,
                "reason": "API error (status 503)",
            })),
            Some("API error (status 503) (after 3 attempts)".to_owned())
        );
        assert_eq!(
            retry_failure(&json!({
                "sessionUpdate": "retry_state",
                "type": "failed",
                "error_type": "auth",
            })),
            Some("the sign-in was rejected".to_owned())
        );
        // A streak still in progress is not a failure yet.
        assert_eq!(
            retry_failure(&json!({
                "sessionUpdate": "retry_state",
                "type": "retrying",
                "attempt": 1,
                "reason": "API error (status 500)",
            })),
            None
        );
        assert_eq!(retry_failure(&json!({ "sessionUpdate": "turn_completed" })), None);
    }

    #[test]
    fn edit_with_diff_content_maps_to_edit_file_and_carries_diff() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "kind": "edit",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "/w/src/main.rs",
                "oldText": "fn old() {}",
                "newText": "fn new() {}",
            }],
        });
        let events = map_update(&update);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::EditFile {
                    path: "/w/src/main.rs".into(),
                    old_string: Some("fn old() {}".into()),
                    new_string: Some("fn new() {}".into()),
                },
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "t2".into(),
                is_error: false,
                output: None,
                diff: Some(ToolDiff {
                    path: "/w/src/main.rs".into(),
                    old_text: Some("fn old() {}".into()),
                    new_text: "fn new() {}".into(),
                }),
            }
        );
    }

    #[test]
    fn new_file_diff_maps_to_write_file() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "kind": "edit",
            "content": [{ "type": "diff", "path": "/w/new.rs", "newText": "x" }],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WriteFile {
                    path: "/w/new.rs".into(),
                    content: None
                },
            }]
        );
    }

    #[test]
    fn status_only_update_resolves_without_refreshing_call() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t4",
            "status": "failed",
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolResult {
                id: "t4".into(),
                is_error: true,
                output: None,
                diff: None,
            }]
        );
    }

    #[test]
    fn plan_maps_to_stable_todo_chip() {
        let update = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read code", "priority": "high", "status": "completed" },
                { "content": "write fix", "priority": "high", "status": "in_progress" },
            ],
        });
        let events = map_update(&update);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: zeron_proto::LIVE_PLAN_TOOL_ID.into(),
                call: ToolCall::Todo {
                    items: vec![
                        TodoItem::new("read code", TodoStatus::Completed),
                        TodoItem::new("write fix", TodoStatus::InProgress),
                    ]
                },
            }
        );
    }

    #[test]
    fn available_commands_parse_with_hint() {
        let update = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "compact", "description": "Compact the session" },
                { "name": "goal", "description": "Set a goal", "input": { "hint": "the goal" } },
                { "description": "nameless is dropped" },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::AvailableCommands {
                commands: vec![
                    SlashCommand {
                        name: "compact".into(),
                        description: "Compact the session".into(),
                        input_hint: None,
                    },
                    SlashCommand {
                        name: "goal".into(),
                        description: "Set a goal".into(),
                        input_hint: Some("the goal".into()),
                    },
                ]
            }]
        );
    }

    #[test]
    fn output_and_diff_caps_apply() {
        let big = "x".repeat(OUTPUT_CAP + 100);
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t5",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": big } },
            ],
        });
        let events = map_update(&update);
        // The content-bearing update refreshes the call, then resolves it.
        let Some(AgentEvent::ToolResult {
            output: Some(output),
            ..
        }) = events.last()
        else {
            panic!("expected resolved result, got {events:?}");
        };
        assert!(output.len() < OUTPUT_CAP + 32);
        assert!(output.ends_with("… [truncated]"));
    }

    #[test]
    fn permission_options_prefer_allow_always() {
        let options = vec![
            json!({ "optionId": "once", "name": "Allow once", "kind": "allow_once" }),
            json!({ "optionId": "always", "name": "Always", "kind": "allow_always" }),
            json!({ "optionId": "no", "name": "Reject", "kind": "reject_once" }),
        ];
        assert_eq!(preferred_allow_option(&options), Some("always".into()));
        let only_reject = vec![json!({ "optionId": "no", "kind": "reject_once" })];
        assert_eq!(preferred_allow_option(&only_reject), Some("no".into()));
        assert_eq!(preferred_allow_option(&[]), None);
    }

    /// Cursor ACP opens tool cards with a display `title` before `rawInput`
    /// is filled (Search "grep", Read "Read File", Web "Web Fetch"). Those
    /// labels must not become typed args.
    #[test]
    fn cursor_placeholder_titles_are_not_typed_args() {
        let grep = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "grep",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&grep),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );

        let read = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t2",
            "title": "Read File",
            "kind": "read",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&read),
            vec![AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::ReadFile {
                    path: String::new(),
                },
            }]
        );

        let fetch = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "title": "Web Fetch",
            "kind": "fetch",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&fetch),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WebSearch {
                    query: String::new(),
                },
            }]
        );

        let web = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t4",
            "title": "Web Search",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&web),
            vec![AgentEvent::ToolCall {
                id: "t4".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );
    }

    #[test]
    fn cursor_search_term_maps_to_web_search() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Web Search: \"multitask cli\"",
            "kind": "search",
            "rawInput": { "searchTerm": "multitask cli" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::WebSearch {
                    query: "multitask cli".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_exec_title_unwraps_markdown_backtick_escapes() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "`echo \\`hi\\``",
            "kind": "execute",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "echo `hi`".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_task_uses_description_not_placeholder_title() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Task: Subagent task",
            "kind": "other",
            "rawInput": {
                "_toolName": "task",
                "description": "Look up multitask docs",
                "prompt": "find the reminder",
            },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Unknown {
                    name: "Task: Look up multitask docs".into(),
                    input: Some(json!({
                        "_toolName": "task",
                        "description": "Look up multitask docs",
                        "prompt": "find the reminder",
                    })),
                },
            }]
        );
    }

    #[test]
    fn opencode_task_keeps_agent_naming_across_frames() {
        // The rawInput frame (in_progress) names the chip off the spawn args.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "in_progress",
            "kind": "think",
            "title": "Viz probe",
            "rawInput": {
                "description": "Viz probe",
                "prompt": "run the probe",
                "subagent_type": "general",
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. }]
                if name == "Agent: Viz probe"
        ));
        // The completion drops rawInput (title = the bare description); the
        // rawOutput metadata still marks the spawn — naming survives.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "title": "Viz probe",
            "content": [{"type": "content", "content": {"type": "text",
                "text": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>"}}],
            "rawOutput": {
                "output": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"parentSessionId": "ses_p", "sessionId": "ses_c"},
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [
                AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. },
                AgentEvent::ToolResult { is_error: false, .. },
            ] if name == "Agent: Viz probe"
        ));
    }
}

#[cfg(test)]
#[test]
fn empty_assistant_delta_does_not_open_a_segment() {
    assert!(
        map_update(&serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": ""},
        }))
        .is_empty()
    );
}
