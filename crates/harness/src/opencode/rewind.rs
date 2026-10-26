//! OpenCode session revert over the native HTTP API.

use serde_json::{json, Value};
use tracing::warn;

use super::Server;
use crate::{HarnessError, RevertTarget};

pub(super) async fn preview(
    server: &Server,
    session_id: &str,
    directory: Option<&str>,
    target: &RevertTarget,
) -> Result<Vec<String>, HarnessError> {
    let message_id = locate(server, session_id, directory, target).await?;
    diff_paths(server, session_id, directory, &message_id).await
}

pub(super) async fn commit(
    server: &Server,
    session_id: &str,
    directory: Option<&str>,
    target: &RevertTarget,
    files: bool,
) -> Result<(), HarnessError> {
    let message_id = locate(server, session_id, directory, target).await?;
    let path = format!("/session/{session_id}/revert");
    let body = json!({ "messageID": message_id, "files": files });
    match server.post_json(&path, directory, &body).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let fallback = format!("/session/{session_id}/message/{message_id}/revert");
            match server.post_json(&fallback, directory, &json!({})).await {
                Ok(_) => Ok(()),
                Err(_) => Err(error),
            }
        }
    }
}

async fn locate(
    server: &Server,
    session_id: &str,
    directory: Option<&str>,
    target: &RevertTarget,
) -> Result<String, HarnessError> {
    let messages = list_messages(server, session_id, directory).await?;
    let turns = user_turns(&messages);
    let turn = crate::revert::pick_turn(&turns, target)?;
    let Some(message) = messages.get(turn.key as usize) else {
        return Err(HarnessError::Protocol(
            "could not find that message in the OpenCode session".into(),
        ));
    };
    let Some(id) = message_id(message) else {
        return Err(HarnessError::Protocol(
            "OpenCode message is missing an id".into(),
        ));
    };
    Ok(id.to_owned())
}

fn user_turns(messages: &[Value]) -> Vec<crate::revert::UserTurn> {
    let mut turns = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if !is_user(message) {
            continue;
        }
        let Some(text) = message_text(message) else {
            continue;
        };
        turns.push(crate::revert::UserTurn {
            key: index as u64,
            text,
            id: message_id(message).map(str::to_owned),
        });
    }
    turns
}

fn is_user(message: &Value) -> bool {
    let role = message
        .pointer("/info/role")
        .and_then(Value::as_str)
        .or_else(|| message.get("role").and_then(Value::as_str));
    role == Some("user")
}

fn message_id(message: &Value) -> Option<&str> {
    message
        .pointer("/info/id")
        .and_then(Value::as_str)
        .or_else(|| message.get("id").and_then(Value::as_str))
}

fn message_text(message: &Value) -> Option<String> {
    if let Some(text) = message
        .pointer("/info/content")
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    let mut parts = Vec::new();
    collect_text(&mut parts, message.get("parts"));
    collect_text(&mut parts, message.pointer("/info/parts"));
    let joined = parts.join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn collect_text(parts: &mut Vec<String>, value: Option<&Value>) {
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };
    for chunk in items {
        let kind = chunk.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "file" || kind == "patch" || kind == "step-start" || kind == "step-finish" {
            continue;
        }
        if let Some(text) = chunk.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                parts.push(text.to_owned());
            }
        }
    }
}

async fn list_messages(
    server: &Server,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Vec<Value>, HarnessError> {
    let path = format!("/session/{session_id}/message");
    let body = server.get_json(&path, directory).await?;
    if let Some(items) = body.as_array() {
        return Ok(items.clone());
    }
    match body.get("messages").and_then(Value::as_array) {
        Some(items) => Ok(items.clone()),
        None => Ok(Vec::new()),
    }
}

async fn diff_paths(
    server: &Server,
    session_id: &str,
    directory: Option<&str>,
    message_id: &str,
) -> Result<Vec<String>, HarnessError> {
    let path = format!("/session/{session_id}/diff?messageID={message_id}");
    let body = match server.get_json(&path, directory).await {
        Ok(body) => body,
        Err(error) => {
            warn!(%error, "opencode session diff unavailable");
            return Ok(Vec::new());
        }
    };
    let mut out = Vec::new();
    collect_paths(&mut out, &body);
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_paths(out: &mut Vec<String>, value: &Value) {
    if let Some(items) = value.as_array() {
        for item in items {
            collect_paths(out, item);
        }
        return;
    }
    let Some(map) = value.as_object() else {
        return;
    };
    for key in ["path", "file", "filename", "relativePath"] {
        if let Some(path) = map.get(key).and_then(Value::as_str) {
            if !path.is_empty() {
                out.push(path.to_owned());
            }
        }
    }
    if let Some(nested) = map.get("files").or_else(|| map.get("diffs")) {
        collect_paths(out, nested);
    }
}
