//! Codex rollout parser.
//!
//! Layout confirmed against 55 real rollouts (44,454 records, 9,692 function
//! calls) on 2026-09-27. The file format is undocumented and version-stamped
//! (`cli_version` in session_meta), so every branch here degrades to
//! `Unknown` rather than guessing.
//!
//! Record envelope:  { timestamp, type, payload }
//!   type=session_meta     payload{ id, cwd, cli_version, git{commit_hash,branch,repository_url} }
//!   type=response_item    payload.type in {
//!       function_call,        { name, arguments(JSON string), call_id }
//!       function_call_output, { call_id, output(JSON string) }
//!       custom_tool_call,     { name, input, status, call_id }
//!       message,              { role, content[], phase }
//!       reasoning,            { encrypted_content }  <- unreadable by design, ignored
//!   }
//!   type=event_msg        payload.type in { agent_message, exec_command_end,
//!                                         patch_apply_end, turn_aborted, error, ... }
//!
//! Two facts do the heavy lifting:
//!   - `session_meta.payload.git.commit_hash` anchors the session to a commit, so
//!     file attribution is exact rather than inferred.
//!   - `call_id` joins a call to its output, which is where the exit code lives.

use crate::record::*;
use serde_json::Value;
use std::path::Path;

/// File extensions a Codex rollout can live behind.
pub const ROLLOUT_EXT: &str = "jsonl";

/// Does this path look like a Codex rollout?
pub fn looks_like_rollout(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some(ROLLOUT_EXT)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout-"))
            .unwrap_or(false)
}

pub struct CodexParse {
    pub session: Session,
    /// call_id -> exit code, filled as outputs stream past.
    exits: std::collections::HashMap<String, i32>,
    /// call_id -> index into session.actions
    index: std::collections::HashMap<String, usize>,
    /// Most recent final-phase assistant text, in transcript order.
    last_final_claim: Option<String>,
    /// Fallback when no `phase: final` exists: the last assistant text of any phase.
    last_assistant_text: Option<String>,
}

/// Parse one rollout file. Never panics on malformed input: unparseable lines
/// are counted in `parse.records_malformed` and skipped.
pub fn parse_file(path: &Path) -> Result<CodexParse, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut st = CodexParse {
        session: Session::new(AgentKind::Codex, "unknown", path.display().to_string()),
        exits: std::collections::HashMap::new(),
        index: std::collections::HashMap::new(),
        last_final_claim: None,
        last_assistant_text: None,
    };
    let mut seq: u32 = 0;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        st.session.parse.records_seen += 1;
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            st.session.parse.records_malformed += 1;
            continue;
        };
        st.session.parse.records_parsed += 1;

        let ts = rec
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let rtype = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let Some(payload) = rec.get("payload") else {
            st.session.parse.payload_unusable += 1;
            continue;
        };
        let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match (rtype, ptype) {
            // ---- session metadata: the git anchor ----
            ("session_meta", _) | (_, "") => {
                if payload.get("id").is_some() {
                    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                        st.session.session_id = id.to_owned();
                    }
                    st.session.cwd = str_field(payload, "cwd");
                    st.session.cli_version = str_field(payload, "cli_version");
                    if let Some(g) = payload.get("git") {
                        st.session.baseline_commit = str_field(g, "commit_hash");
                        st.session.branch = str_field(g, "branch");
                        st.session.repo_url = str_field(g, "repository_url");
                    }
                    st.session.started_at = ts.clone();
                } else {
                    st.session.parse.payload_unusable += 1;
                }
            }
            ("turn_context", _) => {
                st.session.parse.records_unknown_type += 1;
            }
            // ---- a function/tool call: the action ----
            ("response_item", "function_call") => {
                let tool = str_field(payload, "name").unwrap_or_default();
                let call_id = str_field(payload, "call_id");
                let (command, workdir, files) = interpret_args(
                    &tool,
                    payload.get("arguments"),
                    &mut st.session.parse.payload_unusable,
                );
                let action = AgentAction {
                    schema_version: SCHEMA_VERSION,
                    session_id: st.session.session_id.clone(),
                    seq,
                    ts: ts.clone(),
                    kind: classify(&tool),
                    tool: tool.clone(),
                    command,
                    files,
                    outcome: Outcome::Unknown,
                    exit_code: None,
                    workdir,
                    call_id: call_id.clone(),
                };
                if let Some(id) = &call_id {
                    st.index.insert(id.clone(), st.session.actions.len());
                }
                st.session.actions.push(action);
                seq += 1;
            }
            ("response_item", "custom_tool_call") => {
                let tool = str_field(payload, "name").unwrap_or_default();
                let call_id = str_field(payload, "call_id");
                let input = str_field(payload, "input").unwrap_or_default();
                let files = parse_patch_files(&input);
                let action = AgentAction {
                    schema_version: SCHEMA_VERSION,
                    session_id: st.session.session_id.clone(),
                    seq,
                    ts: ts.clone(),
                    kind: classify(&tool),
                    tool,
                    command: None,
                    files,
                    outcome: Outcome::Unknown,
                    exit_code: None,
                    workdir: None,
                    call_id: call_id.clone(),
                };
                if let Some(id) = &call_id {
                    st.index.insert(id.clone(), st.session.actions.len());
                }
                st.session.actions.push(action);
                seq += 1;
            }
            // ---- the outcome, correlated by call_id ----
            ("response_item", "function_call_output") => {
                if let Some(id) = str_field(payload, "call_id") {
                    if let Some(code) = extract_exit_code(payload.get("output")) {
                        st.exits.insert(id.clone(), code);
                        if let Some(&i) = st.index.get(&id) {
                            st.session.actions[i].exit_code = Some(code);
                        }
                    } else {
                        st.session.parse.payload_unusable += 1;
                    }
                }
            }
            // ---- the claim ----
            ("response_item", "message") => {
                let role = str_field(payload, "role").unwrap_or_default();
                let text = collect_text(payload.get("content"));
                if role == "assistant" && !text.trim().is_empty() {
                    let phase = str_field(payload, "phase").unwrap_or_default();
                    st.last_assistant_text = Some(text.clone());
                    if phase == "final" {
                        st.last_final_claim = Some(text);
                    }
                }
                st.session.ended_at = ts.clone();
            }
            // ---- recognised but not needed for the verdict ----
            // These carry no evidence about what the agent did or claimed. We
            // name them explicitly so they count as understood rather than
            // dragging coverage down, and so an unrecognised type still stands
            // out. Verified against the 2026-09-27 corpus census.
            ("response_item", "reasoning")
            | ("event_msg", "token_count")
            | ("event_msg", "item_completed")
            | ("event_msg", "item_started")
            | ("event_msg", "task_started")
            | ("event_msg", "task_complete")
            | ("event_msg", "context_compacted")
            | ("event_msg", "thread_settings_applied")
            | ("response_item", "reasoning_summary")
            | ("response_item", "web_search_call")
            | ("response_item", "custom_tool_call_output")
            | ("event_msg", "custom_tool_call_output")
            | ("event_msg", "user_message")
            | ("event_msg", "agent_reasoning")
            | ("event_msg", "agent_reasoning_delta")
            | ("event_msg", "agent_message_delta") => {
                // Recognised. Carries no evidence about what was done or claimed,
                // and `records_parsed` was already incremented, so the only thing
                // required here is to NOT count it as unknown.
            }
            // ---- lifecycle signals ----
            ("event_msg", "agent_message") => {
                if let Some(t) = str_field(payload, "message") {
                    if str_field(payload, "phase").as_deref() == Some("final") {
                        st.last_final_claim = Some(t);
                    } else {
                        st.last_assistant_text = Some(t);
                    }
                }
            }
            ("event_msg", "error") => {
                let e = str_field(payload, "message").or_else(|| str_field(payload, "error"));
                if let Some(e) = e {
                    st.session.session_error.get_or_insert(e);
                }
            }
            ("event_msg", "turn_aborted") => {
                // The user or a supervisor stopped the turn. That is not a lie and
                // not a runtime failure, so it must not be reported as one. It does
                // mean the working tree may hold half-finished work.
                st.session.aborted = true;
            }
            ("event_msg", "exec_command_end") | ("event_msg", "patch_apply_end") => {
                // Independent corroboration that the action stream is real. We do
                // not use it as a primary signal because it carries no call_id we
                // can join on, and double-counting would inflate coverage.
            }
            // ---- everything else: acknowledged, not modelled ----
            _ => {
                st.session.parse.records_unknown_type += 1;
            }
        }
    }

    // Resolve outcomes from the exit map for anything the streaming pass missed.
    for a in st.session.actions.iter_mut() {
        if a.exit_code.is_none() {
            if let Some(id) = &a.call_id {
                if let Some(code) = st.exits.get(id) {
                    a.exit_code = Some(*code);
                }
            }
        }
        a.outcome = match a.exit_code {
            Some(0) => Outcome::Success,
            Some(_) => Outcome::Failure,
            None => Outcome::Unknown,
        };
        if a.outcome == Outcome::Unknown {
            st.session.parse.outcomes_unknown += 1;
        }
    }

    let claim_text = st
        .last_final_claim
        .take()
        .or_else(|| st.last_assistant_text.take());
    st.session.claim = match claim_text {
        Some(t) => {
            let asserted = crate::claim::extract_paths(&t);
            Claim {
                asserted_files: asserted,
                text: t,
                missing: false,
            }
        }
        None => Claim::default(),
    };
    st.session.claim.missing = st.session.claim.text.is_empty();

    Ok(st)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_owned)
}

/// Concatenate text from a content array, tolerating the several shapes upstream
/// has used (`output_text`, `input_text`, `text`, plain strings).
fn collect_text(content: Option<&Value>) -> String {
    let mut out = String::new();
    let Some(c) = content else { return out };
    if let Some(s) = c.as_str() {
        return s.to_owned();
    }
    let Some(arr) = c.as_array() else { return out };
    for b in arr {
        if let Some(s) = b.as_str() {
            out.push_str(s);
            continue;
        }
        for k in ["text", "content"] {
            if let Some(s) = b.get(k).and_then(|x| x.as_str()) {
                out.push_str(s);
                break;
            }
        }
    }
    out
}

/// Map an upstream tool name to an action class.
fn classify(tool: &str) -> ActionKind {
    match tool {
        "shell_command" | "exec_command" | "local_shell" | "container.exec" => ActionKind::Shell,
        "apply_patch" | "patch" | "edit_file" | "write_file" => ActionKind::Patch,
        "web_search" | "web_search_call" => ActionKind::WebSearch,
        "update_plan" | "create_goal" | "get_goal" | "update_goal" => ActionKind::Plan,
        "" => ActionKind::Unclassified,
        _ => ActionKind::ToolCall,
    }
}

/// `arguments` is a JSON-encoded string. Be tolerant: some records inline an
/// object instead. A failure here is counted, not silently swallowed, because a
/// shell command we cannot read must never be mistaken for a command that was
/// never run.
fn interpret_args(
    tool: &str,
    args: Option<&Value>,
    unusable: &mut u64,
) -> (Option<String>, Option<String>, Vec<String>) {
    let parsed = match args {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) => Some(v),
            Err(_) => {
                *unusable += 1;
                None
            }
        },
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(_) => {
            *unusable += 1;
            None
        }
        None => None,
    };
    let Some(p) = parsed else {
        return (None, None, Vec::new());
    };
    let command = ["command", "cmd", "script", "input"]
        .iter()
        .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
        .map(str::to_owned);
    if command.is_none() && classify(tool) == ActionKind::Shell {
        // A shell call with no readable command field.
        *unusable += 1;
    }
    let workdir = ["workdir", "cwd", "working_directory"]
        .iter()
        .find_map(|k| p.get(*k).and_then(|v| v.as_str()))
        .map(str::to_owned);
    let mut files = Vec::new();
    if let Some(c) = &command {
        files.extend(crate::claim::extract_paths(c));
    }
    if let Some(arr) = p.get("paths").and_then(|v| v.as_array()) {
        for it in arr.iter().filter_map(|v| v.as_str()) {
            files.push(it.to_owned());
        }
    }
    let _ = tool;
    files.sort();
    files.dedup();
    (command, workdir, files)
}

/// Pull an exit code out of a `function_call_output` payload.
///
/// `output` is a JSON-encoded string whose inner shape has changed across
/// versions. Try, in order: a top-level `exit_code`, a `metadata.exit_code`, and
/// an OpenAI-style `{"output": "...", "metadata": {...}}` wrapper. Anything else
/// is `Unknown`, never optimistically zero.
pub fn extract_exit_code(output: Option<&Value>) -> Option<i32> {
    fn from_any(v: &Value) -> Option<i32> {
        for k in ["exit_code", "exitCode", "returncode", "code", "status_code"] {
            if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
                return Some(n as i32);
            }
        }
        if let Some(m) = v.get("metadata") {
            if let Some(n) = from_any(m) {
                return Some(n);
            }
        }
        None
    }
    let o = output?;
    if let Some(c) = from_any(o) {
        return Some(c);
    }
    if let Some(s) = o.as_str() {
        if let Ok(inner) = serde_json::from_str::<Value>(s) {
            return from_any(&inner);
        }
    }
    None
}

/// Extract file paths from an `*** Begin Patch` body.
///
/// The apply_patch format names every file it touches on its own line, which
/// makes this the highest-confidence file evidence the transcript offers.
pub fn parse_patch_files(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.lines() {
        let t = line.trim();
        for pfx in [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(rest) = t.strip_prefix(pfx) {
                let p = rest.trim();
                if !p.is_empty() {
                    out.push(p.to_owned());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_rollout_names() {
        assert!(looks_like_rollout(Path::new(
            "rollout-2026-03-25T23-39-57-abc.jsonl"
        )));
        assert!(!looks_like_rollout(Path::new("session.jsonl")));
    }

    #[test]
    fn patch_files_are_extracted() {
        let patch = "*** Begin Patch\n*** Update File: c:\\a\\main.js\n@@\n-old\n+new\n*** Delete File: c:\\a\\gone.js\n*** End Patch";
        let got = parse_patch_files(patch);
        assert_eq!(
            got,
            vec!["c:\\a\\gone.js".to_string(), "c:\\a\\main.js".to_string()]
        );
    }

    #[test]
    fn exit_code_is_found_in_metadata() {
        let v: Value =
            serde_json::from_str(r#"{"output":"hi","metadata":{"exit_code":1}}"#).unwrap();
        assert_eq!(extract_exit_code(Some(&v)), Some(1));
    }

    #[test]
    fn exit_code_absent_is_unknown_not_zero() {
        let v: Value = serde_json::from_str(r#"{"output":"just text"}"#).unwrap();
        assert_eq!(extract_exit_code(Some(&v)), None);
    }

    #[test]
    fn classifies_tools() {
        assert_eq!(classify("shell_command"), ActionKind::Shell);
        assert_eq!(classify("apply_patch"), ActionKind::Patch);
        assert_eq!(classify("update_plan"), ActionKind::Plan);
        assert_eq!(classify("some_new_tool"), ActionKind::ToolCall);
    }

    #[test]
    fn collects_text_from_both_block_shapes() {
        let a: Value = serde_json::from_str(r#"[{"type":"output_text","text":"hello "}]"#).unwrap();
        let b: Value = serde_json::from_str(r#"[{"type":"input_text","text":"world"}]"#).unwrap();
        assert_eq!(collect_text(Some(&a)), "hello ");
        assert_eq!(collect_text(Some(&b)), "world");
    }
}
