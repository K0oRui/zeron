//! Rewind a provider's native session to a Zeron user message.
//!
//! OpenCode owns its store and overrides the harness methods.
//! Everyone else persists JSONL (or nothing): preview reports no restorable
//! files, and commit truncates the file at the target user turn so the next
//! resume does not replay messages the transcript no longer has.

use std::path::{Path, PathBuf};

use zeron_proto::{HarnessId, RunRequest};

use crate::{HarnessError, RevertTarget};

/// One user turn in a provider transcript. `key` is a byte offset in JSONL
/// or a message index in an array store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserTurn {
    pub key: u64,
    pub text: String,
    pub id: Option<String>,
}

pub fn resume_id(request: &RunRequest) -> Option<&str> {
    request.resume.as_deref().filter(|id| !id.is_empty())
}

fn safe_id(id: &str) -> Option<&str> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        return None;
    }
    Some(id)
}

pub fn texts_match(stored: &str, target: &str) -> bool {
    let stored = stored.trim();
    let target = target.trim();
    if stored.is_empty() || target.is_empty() {
        return stored == target;
    }
    if stored == target {
        return true;
    }
    stored.contains(target) || (target.len() >= 24 && target.contains(stored))
}

pub fn pick_turn<'a>(
    turns: &'a [UserTurn],
    target: &RevertTarget,
) -> Result<&'a UserTurn, HarnessError> {
    if !target.message_id.is_empty() {
        for turn in turns {
            if turn.id.as_deref() == Some(target.message_id.as_str()) {
                return Ok(turn);
            }
        }
    }
    if let Some(found) = turns.get(target.user_index) {
        if target.text.is_empty() || texts_match(&found.text, &target.text) {
            return Ok(found);
        }
    }
    let mut found = None;
    let mut extra = false;
    for turn in turns {
        if texts_match(&turn.text, &target.text) {
            if found.is_some() {
                extra = true;
                break;
            }
            found = Some(turn);
        }
    }
    match (found, extra) {
        (Some(turn), false) => Ok(turn),
        (None, _) => Err(HarnessError::Protocol(
            "could not find that message in the provider session".into(),
        )),
        _ => turns.get(target.user_index).ok_or_else(|| {
            HarnessError::Protocol("could not find that message in the provider session".into())
        }),
    }
}

pub fn preview(
    harness: HarnessId,
    request: &RunRequest,
    target: &RevertTarget,
) -> Result<Vec<String>, HarnessError> {
    preview_at(session_file(harness, request).as_deref(), target)
}

pub fn commit(
    harness: HarnessId,
    request: &RunRequest,
    target: &RevertTarget,
    _files: bool,
) -> Result<(), HarnessError> {
    commit_at(session_file(harness, request).as_deref(), target)
}

pub fn preview_at(file: Option<&Path>, target: &RevertTarget) -> Result<Vec<String>, HarnessError> {
    let Some(path) = file else {
        return Ok(Vec::new());
    };
    let turns = jsonl_user_turns(path)?;
    if turns.is_empty() {
        return Ok(Vec::new());
    }
    pick_turn(&turns, target)?;
    Ok(Vec::new())
}

pub fn commit_at(file: Option<&Path>, target: &RevertTarget) -> Result<(), HarnessError> {
    let Some(path) = file else {
        return Ok(());
    };
    let turns = jsonl_user_turns(path)?;
    if turns.is_empty() {
        return Ok(());
    }
    let turn = pick_turn(&turns, target)?;
    truncate_at(path, turn.key)
}

pub fn session_file(harness: HarnessId, request: &RunRequest) -> Option<PathBuf> {
    session_file_in(harness, request, None)
}

pub fn session_file_in(
    harness: HarnessId,
    request: &RunRequest,
    extra_root: Option<&Path>,
) -> Option<PathBuf> {
    let id = safe_id(resume_id(request)?)?;
    match harness {
        HarnessId::Opencode | HarnessId::Mock => None,
        HarnessId::ClaudeCode => claude_session(&request.env, id),
        HarnessId::Codex => codex_rollout(&request.env, id),
        HarnessId::Pi => None,
        HarnessId::Cursor => cursor_session(id),
        HarnessId::Grok => {
            let defaults = [home().join(".grok").join("sessions")];
            find_named(&roots_for(extra_root, &defaults), id, 3)
        }
        HarnessId::Antigravity => {
            let defaults = [home().join(".gemini"), home().join(".antigravity")];
            find_named(&roots_for(extra_root, &defaults), id, 4)
        }
        HarnessId::Devin => {
            let defaults = [home().join(".devin")];
            find_named(&roots_for(extra_root, &defaults), id, 4)
        }
        HarnessId::Hermes => {
            let defaults = [home().join(".hermes"), home().join(".nous")];
            find_named(&roots_for(extra_root, &defaults), id, 4)
        }
        HarnessId::Kilo => {
            let defaults = [home().join(".kilo"), home().join(".kilocode")];
            find_named(&roots_for(extra_root, &defaults), id, 4)
        }
        HarnessId::Muse => {
            let roots = [home().join(".muse"), home().join(".local/share/muse")];
            find_named(&roots, id, 4)
        }
    }
}

fn home() -> PathBuf {
    crate::executable::home_or_current_dir()
}

fn roots_for(extra: Option<&Path>, defaults: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = extra {
        roots.push(root.to_path_buf());
    }
    roots.extend_from_slice(defaults);
    roots
}

fn env_path(env: &std::collections::BTreeMap<String, String>, key: &str) -> Option<PathBuf> {
    env.get(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn claude_session(
    env: &std::collections::BTreeMap<String, String>,
    session_id: &str,
) -> Option<PathBuf> {
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let root = env_path(env, "CLAUDE_CONFIG_DIR").unwrap_or_else(|| {
        env_path(env, "HOME")
            .map(|home| home.join(".claude"))
            .unwrap_or_else(|| home().join(".claude"))
    });
    let projects = std::fs::read_dir(root.join("projects")).ok()?;
    projects.flatten().find_map(|project| {
        let path = project.path().join(format!("{session_id}.jsonl"));
        path.is_file().then_some(path)
    })
}

fn codex_rollout(
    env: &std::collections::BTreeMap<String, String>,
    thread_id: &str,
) -> Option<PathBuf> {
    let home = env_path(env, "CODEX_HOME").unwrap_or_else(|| home().join(".codex"));
    let roots = [
        home.join("sessions"),
        home.join("archived_sessions"),
        home,
    ];
    find_named(&roots, thread_id, 4)
}

fn cursor_session(id: &str) -> Option<PathBuf> {
    let root = std::env::var_os("ZERON_CURSOR_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".zeron").join("cursor-state"));
    let marker = root.join("by-agent").join(id);
    let dir = PathBuf::from(std::fs::read_to_string(marker).ok()?.trim());
    if !dir.is_dir() {
        return None;
    }
    let roots = [dir];
    find_named(&roots, id, 3)
}

fn find_named(roots: &[PathBuf], id: &str, max_depth: u32) -> Option<PathBuf> {
    let mut seen = 0;
    let mut stack: Vec<(PathBuf, u32)> = roots.iter().cloned().map(|path| (path, 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 20_000 {
                return None;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) == Some(id) {
                    let history = path.join("chat_history.jsonl");
                    if history.is_file() {
                        return Some(history);
                    }
                    if let Some(found) = first_jsonl(&path) {
                        return Some(found);
                    }
                }
                if depth < max_depth {
                    stack.push((path, depth + 1));
                }
            } else if kind.is_file() {
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
                if name == format!("{id}.jsonl") || (name.ends_with(".jsonl") && name.contains(id))
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn first_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") && path.is_file() {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

pub fn jsonl_user_turns(path: &Path) -> Result<Vec<UserTurn>, HarnessError> {
    use std::io::BufRead as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut reader = std::io::BufReader::with_capacity(1 << 16, file);
    let mut line = Vec::new();
    let mut offset = 0u64;
    let mut turns = Vec::new();
    loop {
        line.clear();
        let start = offset;
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Err(error) => return Err(error.into()),
            Ok(n) => offset += n as u64,
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(prompt) = classify_user(&value) else {
            continue;
        };
        turns.push(UserTurn {
            key: start,
            text: prompt.text,
            id: prompt.id,
        });
    }
    Ok(turns)
}

struct Prompt {
    id: Option<String>,
    text: String,
}

fn classify_user(value: &serde_json::Value) -> Option<Prompt> {
    if value.get("parent_tool_use_id").is_some_and(|v| !v.is_null()) {
        return None;
    }
    if is_tool_result_only(value) {
        return None;
    }
    let role = value
        .pointer("/message/role")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.pointer("/payload/role").and_then(serde_json::Value::as_str))
        .or_else(|| value.pointer("/role").and_then(serde_json::Value::as_str));
    let typ = value.get("type").and_then(serde_json::Value::as_str);
    let payload_type = value
        .pointer("/payload/type")
        .and_then(serde_json::Value::as_str);
    let is_user = role == Some("user")
        || typ == Some("user")
        || typ == Some("user_message")
        || payload_type == Some("user_message");
    if !is_user {
        return None;
    }
    let text = extract_text(value)?;
    if text.trim().is_empty() {
        return None;
    }
    let id = value
        .get("uuid")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("id").and_then(serde_json::Value::as_str))
        .or_else(|| value.pointer("/payload/id").and_then(serde_json::Value::as_str))
        .or_else(|| value.pointer("/message/id").and_then(serde_json::Value::as_str))
        .map(str::to_owned);
    Some(Prompt { id, text })
}

fn is_tool_result_only(value: &serde_json::Value) -> bool {
    let Some(items) = value
        .pointer("/message/content")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    for item in items {
        if item.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
            return false;
        }
    }
    true
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value
        .pointer("/message/content")
        .and_then(serde_json::Value::as_str)
    {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = value
        .pointer("/payload/message")
        .and_then(serde_json::Value::as_str)
    {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = value.get("content").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    for path in ["/message/content", "/payload/content", "/content", "/parts"] {
        if let Some(items) = value.pointer(path).and_then(serde_json::Value::as_array) {
            let mut parts = Vec::new();
            for item in items {
                if let Some(text) = item.as_str() {
                    if !text.is_empty() {
                        parts.push(text.to_owned());
                    }
                    continue;
                }
                let kind = item
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if kind == "tool_result" || kind == "tool_use" || kind == "image" || kind == "thinking"
                {
                    continue;
                }
                if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                    if !text.is_empty() {
                        parts.push(text.to_owned());
                    }
                }
            }
            let joined = parts.join("\n");
            if !joined.trim().is_empty() {
                return Some(joined);
            }
        }
    }
    None
}

fn truncate_at(path: &Path, offset: u64) -> Result<(), HarnessError> {
    use std::io::{Read as _, Seek as _, Write as _};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let len = file.metadata()?.len();
    if offset >= len {
        return Ok(());
    }
    let mut newline = false;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset - 1))?;
        let mut tail = [0u8; 1];
        newline = file.read_exact(&mut tail).is_ok() && tail[0] == b'\n';
    }
    file.set_len(offset)?;
    if offset > 0 && !newline {
        file.seek(std::io::SeekFrom::End(0))?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn target(index: usize, text: &str) -> RevertTarget {
        RevertTarget {
            message_id: String::new(),
            user_index: index,
            text: text.into(),
        }
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn jsonl_truncates_at_the_second_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"type":"user","uuid":"a","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
                r#"{"type":"assistant","message":{"content":"ok"}}"#,
                r#"{"type":"user","uuid":"b","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
                r#"{"type":"assistant","message":{"content":"later"}}"#,
            ],
        );
        commit_at(Some(&path), &target(1, "second")).unwrap();
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.contains("first"), "{kept}");
        assert!(!kept.contains("second"), "{kept}");
        assert!(!kept.contains("later"), "{kept}");
    }

    #[test]
    fn jsonl_skips_tool_result_only_user_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"type":"user","uuid":"a","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#,
                r#"{"type":"user","uuid":"tool","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
                r#"{"type":"user","uuid":"b","message":{"role":"user","content":[{"type":"text","text":"again"}]}}"#,
            ],
        );
        let turns = jsonl_user_turns(&path).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].text, "again");
    }
}
