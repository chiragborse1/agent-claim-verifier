//! Normalised record types.
//!
//! This is the architectural load-bearing file. v1 of the tool is a read-only
//! verifier; v2 is a signed, append-only audit ledger. The schema here is the
//! contract between them. If this changes when v2 arrives, the architecture was
//! wrong.
//!
//! Rules that are not negotiable:
//!   - `parse` stats are always present. A parser that silently under-reports is
//!     worse than no tool, because it manufactures confidence.
//!   - Absence of evidence is `Unknown`, never `Success`. See `Outcome`.

use serde::{Deserialize, Serialize};

/// Bump when the on-disk record layout changes incompatibly.
pub const SCHEMA_VERSION: u16 = 1;

/// Which agent runtime produced the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Codex,
    ClaudeCode,
    /// A transcript layout we have seen but do not yet model.
    Unknown,
}

impl AgentKind {
    pub fn label(&self) -> &'static str {
        match self {
            AgentKind::Codex => "codex",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Unknown => "unknown",
        }
    }
}

/// What the agent actually did, as distinct from what it said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Ran a shell command.
    Shell,
    /// Applied a patch. The patch text names the files, so this is high-confidence.
    Patch,
    /// Some other tool/function call we record but do not deeply interpret.
    ToolCall,
    WebSearch,
    /// Plan/goal bookkeeping, not a side effect.
    Plan,
    /// We saw a record we could not classify.
    Unclassified,
}

impl ActionKind {
    /// Whether this action could plausibly change the repository.
    pub fn mutating(&self) -> bool {
        matches!(
            self,
            ActionKind::Shell | ActionKind::Patch | ActionKind::ToolCall
        )
    }
}

/// Result of an action, derived from exit codes.
///
/// `Unknown` is the load-bearing variant. A session that never ran is not a
/// session that succeeded quietly, and conflating the two is the single worst
/// bug this tool can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failure,
    /// No exit code was recoverable. Says nothing about whether it worked.
    Unknown,
}

/// One normalised agent action. The unit of the future ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAction {
    pub schema_version: u16,
    pub session_id: String,
    /// Monotonic within a session, in transcript order.
    pub seq: u32,
    /// ISO-8601 as recorded upstream. Kept as a string so schema changes in the
    /// source formats never force a migration here.
    pub ts: Option<String>,
    pub kind: ActionKind,
    /// Upstream tool name, e.g. `shell_command`, `apply_patch`.
    pub tool: String,
    /// The command line, for shell actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Files this action is known to touch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// The upstream correlation id, so an action can be joined to its output and
    /// to future ledger entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// The agent's own statement about what it did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    /// The final assistant message text.
    pub text: String,
    /// Files the claim asserts were changed. Extracted by heuristic, so it is
    /// always a subset of what the claim literally names.
    pub asserted_files: Vec<String>,
    /// True when we could not find any final-phase message at all.
    pub missing: bool,
}

impl Default for Claim {
    /// An empty claim is a *missing* claim. Deriving `Default` would leave
    /// `missing: false` alongside empty text, and every consumer would then have
    /// to guess which field to believe.
    fn default() -> Self {
        Claim {
            text: String::new(),
            asserted_files: Vec::new(),
            missing: true,
        }
    }
}

impl Claim {
    /// Whether there is a real claim to compare the world against. Derived from
    /// the text, not trusted from the flag, so an inconsistent struct cannot
    /// cause a false accusation.
    pub fn is_present(&self) -> bool {
        !self.missing && !self.text.trim().is_empty()
    }
}

/// Honest accounting of how much of a transcript we actually understood.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseStats {
    pub records_seen: u64,
    pub records_parsed: u64,
    /// Records of a type we do not model.
    pub records_unknown_type: u64,
    /// Lines that were not valid JSON.
    pub records_malformed: u64,
    /// Actions whose outcome we could not determine.
    pub outcomes_unknown: u64,
    /// Records that matched a modelled type but whose payload was unusable.
    pub payload_unusable: u64,
}

impl ParseStats {
    /// Fraction of transcript records we actually *interpreted*, 0.0..=1.0.
    ///
    /// Deliberately penalises records of an unmodelled type. A metric that
    /// counted "JSON parsed OK" as "understood" would report 100% coverage on a
    /// transcript whose tool calls we had silently ignored, which is the single
    /// most misleading number this tool could print.
    pub fn coverage(&self) -> f64 {
        if self.records_seen == 0 {
            return 0.0;
        }
        let understood = self
            .records_seen
            .saturating_sub(self.records_unknown_type)
            .saturating_sub(self.records_malformed);
        understood as f64 / self.records_seen as f64
    }
}

/// A parsed session, before verification against git.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: u16,
    pub agent: AgentKind,
    pub session_id: String,
    pub source_path: String,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// The commit the session started from. This is what makes exact git
    /// attribution possible instead of guesswork.
    pub baseline_commit: Option<String>,
    pub repo_url: Option<String>,
    pub cli_version: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub actions: Vec<AgentAction>,
    pub claim: Claim,
    pub parse: ParseStats,
    /// Set when the session could not be meaningfully evaluated at all, e.g. the
    /// runtime never authenticated so the agent never acted.
    pub session_error: Option<String>,
    /// The turn was stopped by the user or a supervisor. Not a failure and not a
    /// lie, but the working tree may hold half-finished work.
    pub aborted: bool,
}

impl Session {
    pub fn new(
        agent: AgentKind,
        session_id: impl Into<String>,
        source_path: impl Into<String>,
    ) -> Self {
        Session {
            schema_version: SCHEMA_VERSION,
            agent,
            session_id: session_id.into(),
            source_path: source_path.into(),
            cwd: None,
            branch: None,
            baseline_commit: None,
            repo_url: None,
            cli_version: None,
            started_at: None,
            ended_at: None,
            actions: Vec::new(),
            claim: Claim::default(),
            parse: ParseStats::default(),
            session_error: None,
            aborted: false,
        }
    }

    /// True when no action in the session could have changed anything.
    pub fn no_mutating_actions(&self) -> bool {
        !self.actions.iter().any(|a| a.kind.mutating())
    }

    /// The single highest-severity condition we can detect from the transcript
    /// alone: the agent announced completion but demonstrably performed no work.
    ///
    /// This is not hypothetical. Real transcripts exist where the runtime failed
    /// authentication, emitted a synthetic assistant message reading "Not logged
    /// in - please run /login", and recorded zero tool calls. A naive verifier
    /// reads that as a clean session with nothing to report. It is a lie.
    pub fn claimed_completion_without_work(&self) -> bool {
        self.session_error.is_none() && self.claim.is_present() && self.no_mutating_actions()
    }
}

/// Append-only ledger. v1 writes it so the audit trail exists from day one and v2
/// only has to add signatures and transport.
pub fn ledger_path() -> Option<std::path::PathBuf> {
    std::env::var_os("ACV_LEDGER")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".acv")
                        .join("records.jsonl")
                })
        })
}

pub fn append_ledger(lines: &[String]) -> std::io::Result<Option<std::path::PathBuf>> {
    use std::io::Write;
    let Some(path) = ledger_path() else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    Ok(Some(path))
}
