//! Claude Code transcript parser.
//!
//! IMPORTANT, AND NOT OPTIMISTIC: this parser is structurally informed but
//! **empirically unvalidated on this machine**. Every one of the 7 local
//! transcripts turned out to be `authentication_failed` runs that recorded zero
//! tool calls, so there is no local sample of a working session to test against.
//!
//! That is exactly why the coverage number is published per session. A user with
//! real Claude Code sessions should see `records_unknown_type` rise if the format
//! has moved, and the report will say INCONCLUSIVE rather than quietly passing.
//!
//! Layout (observed 2026-09-27, 7 files, 66 records):
//!   { type, uuid, parentUuid, isSidechain, timestamp, cwd, gitBranch,
//!     version, sessionId, ... }
//!   type=assistant            message{ role, content[], model, stop_reason, usage }
//!   type=user                 message.content is usually a raw string
//!   type=attachment           attachment{ type: ... }
//!   type=file-history-snapshot
//!
//! The auth-failure shape, and the reason `session_error` exists:
//!   type=assistant, message.model == "<synthetic>",
//!   isApiErrorMessage == true, error == "authentication_failed",
//!   message.content == [{ type:"text", text:"Not logged in - please run /login" }]

use crate::claim;
use crate::record::*;
use serde_json::Value;
use std::path::Path;

pub fn looks_like_transcript(path: &Path) -> bool {
    // <uuid>.jsonl under a directory named after the cwd
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.len() == 36 && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit()))
            .unwrap_or(false)
}

/// Map a Claude Code tool name to an action class.
pub fn classify(tool: &str) -> ActionKind {
    match tool {
        "Bash" | "BashOutput" => ActionKind::Shell,
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => ActionKind::Patch,
        "WebFetch" | "WebSearch" => ActionKind::WebSearch,
        "TodoWrite" | "ExitPlanMode" | "EnterPlanMode" => ActionKind::Plan,
        "" => ActionKind::Unclassified,
        _ => ActionKind::ToolCall,
    }
}

/// Files an Edit/Write tool call asserts it touched.
fn tool_files(tool: &str, input: Option<&Value>) -> Vec<String> {
    let Some(i) = input else { return Vec::new() };
    let mut out = Vec::new();
    for k in ["file_path", "notebook_path", "path"] {
        if let Some(p) = i.get(k).and_then(|v| v.as_str()) {
            out.push(p.to_owned());
        }
    }
    if out.is_empty() {
        if let Some(p) = i.get("command").and_then(|v| v.as_str()) {
            out.extend(claim::extract_paths(p));
        }
    }
    if out.is_empty() && tool == "Bash" {
        if let Some(c) = i.get("command").and_then(|v| v.as_str()) {
            out.extend(claim::extract_paths(c));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Default)]
struct Acc {
    last_assistant_text: Option<String>,
    last_tool_result_failed: bool,
}

/// Parse a Claude Code transcript. Never panics; counts what it does not model.
pub fn parse_file(path: &Path) -> Result<Session, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_owned();

    let mut s = Session::new(
        AgentKind::ClaudeCode,
        stem.clone(),
        path.display().to_string(),
    );
    let mut acc = Acc::default();
    let mut seq: u32 = 0;
    // The last user-authored prompt is the closest thing to the task statement.
    let mut last_prompt: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        s.parse.records_seen += 1;
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            s.parse.records_malformed += 1;
            continue;
        };
        s.parse.records_parsed += 1;

        let rtype = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = rec
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if ts.is_some() {
            if s.started_at.is_none() {
                s.started_at = ts.clone();
            }
            s.ended_at = ts.clone();
        }
        if s.cwd.is_none() {
            s.cwd = rec.get("cwd").and_then(|v| v.as_str()).map(str::to_owned);
        }
        if s.branch.is_none() {
            s.branch = rec
                .get("gitBranch")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        if s.cli_version.is_none() {
            s.cli_version = rec
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        if let Some(sid) = rec.get("sessionId").and_then(|v| v.as_str()) {
            if s.session_id == "unknown" {
                s.session_id = sid.to_owned();
            }
        }

        let msg = rec.get("message");
        let content = msg.and_then(|m| m.get("content"));

        match rtype {
            "assistant" => {
                // ---- the auth-failure shape ----
                let model = msg
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let is_api_error = rec
                    .get("isApiErrorMessage")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let err = rec.get("error").and_then(|v| v.as_str());
                let text = collect_text(content);
                let low = text.to_ascii_lowercase();
                let looks_dead = is_api_error
                    || model == "<synthetic>"
                    || err.is_some()
                    || low.contains("not logged in")
                    || low.contains("please run /login")
                    || low.contains("authentication");
                if looks_dead {
                    let reason = err
                        .map(str::to_owned)
                        .or_else(|| {
                            let t = text.trim();
                            if t.is_empty() {
                                None
                            } else {
                                Some(t.to_owned())
                            }
                        })
                        .unwrap_or_else(|| "assistant turn was an API error".to_owned());
                    s.session_error.get_or_insert(reason);
                    continue;
                }

                // ---- tool calls ----
                if let Some(arr) = content.and_then(|c| c.as_array()) {
                    for b in arr {
                        let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if bt == "tool_use" {
                            let tool = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            s.actions.push(AgentAction {
                                schema_version: SCHEMA_VERSION,
                                session_id: s.session_id.clone(),
                                seq,
                                ts: ts.clone(),
                                kind: classify(tool),
                                tool: tool.to_owned(),
                                command: b
                                    .get("input")
                                    .and_then(|i| i.get("command"))
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned),
                                files: tool_files(tool, b.get("input")),
                                outcome: Outcome::Unknown,
                                exit_code: None,
                                workdir: None,
                                call_id: b.get("id").and_then(|v| v.as_str()).map(str::to_owned),
                            });
                            seq += 1;
                        } else if bt == "tool_result"
                            && b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)
                        {
                            acc.last_tool_result_failed = true;
                        }
                    }
                }
                if !text.trim().is_empty() {
                    acc.last_assistant_text = Some(text);
                }
            }
            "user" => {
                if rec.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
                    continue;
                }
                let t = collect_text(content);
                // Claude Code wraps local slash commands in <command-name>; those
                // are not task statements.
                if !t.trim().is_empty()
                    && !t.contains("<command-name>")
                    && !t.contains("<local-command-stdout>")
                {
                    last_prompt = Some(t);
                }
            }
            "attachment"
            | "file-history-snapshot"
            | "queue-operation"
            | "system"
            | "mode"
            | "permission-mode"
            | "last-prompt" => {
                s.parse.records_unknown_type += 1;
            }
            _ => {
                s.parse.records_unknown_type += 1;
            }
        }
    }

    // Tool results arriving out of band do not tell us which action they belong
    // to without an id join, so we only use the flag as a session-level signal.
    if acc.last_tool_result_failed {
        s.session_error
            .get_or_insert_with(|| "one or more tool results reported an error".to_owned());
    }

    for a in s.actions.iter_mut() {
        a.outcome = Outcome::Unknown;
        s.parse.outcomes_unknown += 1;
    }

    s.claim = match acc.last_assistant_text {
        Some(t) => Claim {
            asserted_files: claim::extract_paths(&t),
            text: t,
            missing: false,
        },
        None => Claim::default(),
    };
    s.claim.missing = s.claim.text.is_empty();
    let _ = last_prompt;

    Ok(s)
}

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
        } else if let Some(s) = b.get("text").and_then(|v| v.as_str()) {
            out.push_str(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_uuid_stems() {
        assert!(looks_like_transcript(Path::new(
            "3e32d875-cb20-4b6f-9937-b45c58d60d39.jsonl"
        )));
        assert!(!looks_like_transcript(Path::new(
            "rollout-2026-01-01.jsonl"
        )));
    }

    #[test]
    fn classifies_claude_tools() {
        assert_eq!(classify("Bash"), ActionKind::Shell);
        assert_eq!(classify("Edit"), ActionKind::Patch);
        assert_eq!(classify("Read"), ActionKind::ToolCall);
    }

    #[test]
    fn edit_input_yields_file_path() {
        let v: Value = serde_json::from_str(r#"{"file_path":"/repo/src/a.ts"}"#).unwrap();
        assert_eq!(
            tool_files("Edit", Some(&v)),
            vec!["/repo/src/a.ts".to_string()]
        );
    }
}
