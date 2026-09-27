//! acv — agent claim verifier.
//!
//! Reads an AI coding agent's own transcript, compares what it *said* it did
//! against what git and the transcript show actually happened, and reports the
//! difference ranked by how much it can hurt you.
//!
//! Local only. No network calls, no telemetry, no account, no writes outside
//! an optional append-only ledger you can point anywhere with `ACV_LEDGER`.

pub mod claim;
pub mod claude;
pub mod codex;
pub mod git;
pub mod record;
pub mod report;
pub mod verdict;

use record::{AgentKind, Session};
use std::path::{Path, PathBuf};
use verdict::Verdict;

/// One fully evaluated session.
pub struct Evaluated {
    pub session: Session,
    pub facts: git::GitFacts,
    pub verdict: Verdict,
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn default_codex_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ACV_CODEX_DIR") {
        return Some(PathBuf::from(d));
    }
    home().map(|h| h.join(".codex").join("sessions"))
}

pub fn default_claude_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("ACV_CLAUDE_DIR") {
        return Some(PathBuf::from(d));
    }
    home().map(|h| h.join(".claude").join("projects"))
}

/// Walk a directory for candidate transcripts.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(d: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if codex::looks_like_rollout(&p) || claude::looks_like_transcript(&p) {
                out.push(p);
            }
        }
    }
    walk(root, &mut out, 0);
    out
}

fn agent_of(path: &Path) -> AgentKind {
    if codex::looks_like_rollout(path) {
        AgentKind::Codex
    } else {
        AgentKind::ClaudeCode
    }
}

/// Parse one transcript. The source format is chosen by filename shape.
pub fn parse_one(path: &Path) -> Result<Session, String> {
    match agent_of(path) {
        AgentKind::Codex => codex::parse_file(path).map(|p| p.session),
        _ => claude::parse_file(path),
    }
}

/// Evaluate a session against git ground truth.
pub fn evaluate(session: &Session, no_git: bool) -> Evaluated {
    let facts = if no_git {
        git::GitFacts {
            unavailable_reason: Some("--no-git requested".into()),
            ..Default::default()
        }
    } else {
        match session.cwd.as_deref() {
            Some(cwd) => git::collect(Path::new(cwd), session.baseline_commit.as_deref()),
            None => git::GitFacts {
                unavailable_reason: Some(
                    "transcript recorded no cwd, so there is no repo to inspect".into(),
                ),
                ..Default::default()
            },
        }
    };
    let verdict = verdict::evaluate(session, &facts);
    Evaluated {
        session: session.clone(),
        facts,
        verdict,
    }
}

/// Scan a set of transcripts, newest first, capped at `limit`.
pub fn scan(paths: &[PathBuf], limit: usize, no_git: bool) -> (Vec<Evaluated>, Vec<String>) {
    let mut ordered: Vec<PathBuf> = paths.to_vec();
    // Newest first: transcript filenames encode their start timestamp, and
    // mtime is the more reliable signal when present.
    ordered.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    ordered.reverse();
    if limit > 0 {
        ordered.truncate(limit);
    }

    let mut out = Vec::new();
    let mut errs = Vec::new();
    for p in ordered {
        match parse_one(&p) {
            Ok(s) => out.push(evaluate(&s, no_git)),
            Err(e) => errs.push(e),
        }
    }
    (out, errs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_both_formats() {
        let dir = std::env::temp_dir().join(format!("acv-disc-{}", std::process::id()));
        let sub = dir.join("2026").join("03");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("rollout-2026-03-25T23-39-57-abc.jsonl");
        let b = dir.join("3e32d875-cb20-4b6f-9937-b45c58d60d39.jsonl");
        std::fs::write(&a, "{}\n").unwrap();
        std::fs::write(&b, "{}\n").unwrap();
        let found = discover(&dir);
        assert_eq!(found.len(), 2, "found {found:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
