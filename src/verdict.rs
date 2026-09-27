//! The verdict engine.
//!
//! Compares what the agent said against what git and the transcript show. Every
//! finding must be able to answer three questions: what was claimed, what the
//! evidence is, and what the user should do. A finding that cannot is noise.
//!
//! Two rules that override everything else:
//!   1. Absence of evidence is never reported as success.
//!   2. When we could not check, we say so loudly. Silence reads as "all clear",
//!      and "all clear" from a tool that checked nothing is the most dangerous
//!      thing this program could print.

use crate::claim;
use crate::git::{FileStatus, GitFacts};
use crate::record::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The agent said something that is provably not true.
    Critical,
    /// Likely wrong, or right by luck.
    High,
    /// Right, but unverified.
    Medium,
    /// Worth knowing.
    Low,
    /// We could not check.
    Unknown,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Critical => "CRITICAL",
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
            Severity::Unknown => "UNKNOWN",
        }
    }
}

/// A single discrepancy between claim and reality.
#[derive(Debug, Clone)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub title: String,
    /// What the agent said.
    pub claimed: Option<String>,
    /// What we observed.
    pub evidence: String,
    /// What the user should do about it.
    pub action: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Claim agrees with evidence and something actually verified it.
    Verified,
    /// We found at least one discrepancy.
    Discrepancies,
    /// Nothing contradicted the claim, but nothing verified it either.
    Unverified,
    /// The session reported completion while performing no work.
    NoWork,
    /// The session itself failed; there is no claim to judge.
    SessionFailed,
    /// We could not evaluate this session at all.
    Inconclusive,
    /// The turn was stopped before the agent summarised. Not a failure, but the
    /// working tree may hold unreviewed work.
    Aborted,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Verified => "VERIFIED",
            Status::Discrepancies => "DISCREPANCIES",
            Status::Unverified => "UNVERIFIED",
            Status::NoWork => "NO WORK",
            Status::SessionFailed => "SESSION FAILED",
            Status::Inconclusive => "INCONCLUSIVE",
            Status::Aborted => "ABORTED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub status: Status,
    pub findings: Vec<Finding>,
    /// Commands that look like they verified something.
    pub verification_commands: Vec<String>,
    /// How much of the transcript we understood.
    pub coverage: f64,
    /// Files git says changed.
    pub actual_files: Vec<String>,
    /// Files the claim named that we could not check.
    pub unchecked_claim_files: Vec<String>,
}

impl Verdict {
    pub fn critical_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Critical)
            .count()
    }
    pub fn finding_count(&self) -> usize {
        self.findings.len()
    }
}

/// Does this command look like it verifies the work?
pub fn looks_like_verification(cmd: &str) -> bool {
    let c = cmd.to_ascii_lowercase();
    const MARKERS: [&str; 18] = [
        "test",
        "pytest",
        "jest",
        "vitest",
        "mocha",
        "rspec",
        "phpunit",
        "lint",
        "eslint",
        "ruff",
        "mypy",
        "tsc",
        "clippy",
        "typecheck",
        "cargo build",
        "npm run build",
        "make check",
        "ci",
    ];
    MARKERS.iter().any(|m| c.contains(m))
}

/// The first command that looks like it verified the work, with its outcome.
///
/// The outcome travels with the command on purpose. Showing `verified by: pytest`
/// when pytest exited 1 would be a lie told by the reporting layer, which is the
/// one layer a user is guaranteed to believe.
pub fn ran_verification(actions: &[AgentAction]) -> Option<(String, Option<i32>)> {
    actions
        .iter()
        .filter_map(|a| a.command.as_deref().map(|c| (a, c)))
        .find(|(_, c)| looks_like_verification(c))
        .map(|(a, c)| (claim::summarise(c, 90), a.exit_code))
}

/// Render a verification command with its outcome attached. One helper so the
/// early-return path and the main path can never format it differently.
fn format_verification(v: Option<(String, Option<i32>)>) -> Vec<String> {
    v.map(|(cmd, code)| match code {
        Some(0) => format!("{cmd}  (exit 0)"),
        Some(c) => format!("{cmd}  (exit {c})"),
        None => format!("{cmd}  (exit code unknown)"),
    })
    .into_iter()
    .collect()
}

/// The agent's own stated outcome, used for prioritising display.
pub fn claim_headline(session: &Session) -> String {
    if !session.claim.is_present() {
        return "(no final assistant message found)".to_owned();
    }
    claim::summarise(&session.claim.text, 220)
}

/// Run the comparison.
pub fn evaluate(session: &Session, facts: &GitFacts) -> Verdict {
    let mut findings: Vec<Finding> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();

    let actual_files: Vec<String> = facts
        .changed_since_baseline
        .iter()
        .chain(facts.dirty.iter())
        .map(|f| f.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let claimed_files = &session.claim.asserted_files;
    let verification = ran_verification(&session.actions);
    let mut status = Status::Unverified;

    // ---- 1. The session failed. There is no claim to judge. ----
    if let Some(err) = &session.session_error {
        findings.push(Finding {
            id: "session-error",
            severity: Severity::Critical,
            title: "The session failed. Any final message is not a completion claim.".into(),
            claimed: None,
            evidence: format!("transcript recorded: {err}"),
            action: "Fix the session error before trusting anything this session produced. Re-run the task.".into(),
            files: vec![],
        });
        return Verdict {
            status: Status::SessionFailed,
            findings,
            verification_commands: format_verification(verification),
            coverage: session.parse.coverage(),
            actual_files,
            unchecked_claim_files: unchecked,
        };
    }

    // ---- 2. Claimed completion, performed no work. ----
    if session.claimed_completion_without_work() {
        findings.push(Finding {
            id: "no-work",
            severity: Severity::Critical,
            title: "The agent announced completion but ran no commands and applied no patches.".into(),
            claimed: Some(claim_headline(session)),
            evidence: format!(
                "{} transcript records parsed, 0 mutating actions, {} plan/bookkeeping actions",
                session.parse.records_parsed,
                session.actions.iter().filter(|a| !a.kind.mutating()).count()
            ),
            action: "Nothing was done. Do not merge, do not review, do not tell anyone this task is complete.".into(),
            files: vec![],
        });
        status = Status::NoWork;
    }

    // ---- 3. Ended on a failing command. ----
    let mut failures: Vec<&AgentAction> = session
        .actions
        .iter()
        .filter(|a| a.outcome == Outcome::Failure)
        .collect();
    if let Some(last_mut) = session.actions.iter().rev().find(|a| a.kind.mutating()) {
        if last_mut.outcome == Outcome::Failure && !failures.iter().any(|f| f.seq == last_mut.seq) {
            failures.push(last_mut);
        }
    }
    if let Some(f) = failures.last() {
        findings.push(Finding {
            id: "ended-on-failure",
            severity: Severity::High,
            title: "The session's last mutating action exited non-zero.".into(),
            claimed: Some(claim_headline(session)),
            evidence: format!(
                "{} exited {} :: {}",
                f.tool,
                f.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                f.command.as_deref().map(|c| claim::summarise(c, 120)).unwrap_or_else(|| "(no command recorded)".into())
            ),
            action: "Re-run the failing command and read the error. A completion message after a non-zero exit is not evidence of success.".into(),
            files: f.files.clone(),
        });
    }

    // ---- 4 and 5. Claim-versus-reality comparisons. ----
    //
    // These are only meaningful when there IS a claim to compare against. With no
    // final message, "the agent never mentioned this file" is a meaningless
    // accusation: there was no message in which it could have mentioned anything.
    //
    // Observed on real data: 13 of 62 local sessions were aborted by the user, and
    // every one of them would have produced a false "changed files the agent never
    // mentioned" finding under the naive rule.
    let has_claim = session.claim.is_present();
    if facts.available && !has_claim {
        if !actual_files.is_empty() {
            let n = actual_files.len();
            findings.push(Finding {
                id: "changed-without-claim",
                severity: Severity::Medium,
                title: format!("{n} file(s) changed and the session produced no completion claim to check against."),
                claimed: None,
                evidence: actual_files.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                action: "The turn was stopped before the agent summarised. Review these changes \
                         yourself before committing; there is no claim saying they are finished."
                    .to_owned(),
                files: actual_files.clone(),
            });
        }
    } else if facts.available {
        for claimed in claimed_files {
            let matched = actual_files.iter().any(|a| claim::same_file(a, claimed));
            if !matched {
                // Only call it a lie when the file is plausibly inside the repo.
                let outside_repo = !claimed.contains('/')
                    && !claimed.contains('\\')
                    && claimed.rfind('.').map(|i| i < 20).unwrap_or(false);
                if outside_repo {
                    unchecked.push(claimed.clone());
                } else {
                    findings.push(Finding {
                        id: "claimed-untouched",
                        severity: Severity::Medium,
                        title: format!(
                            "The claim names `{claimed}`, but git shows no change to it."
                        ),
                        claimed: Some(claimed.clone()),
                        evidence: if actual_files.is_empty() {
                            "git reports no file changes at all for this session".to_owned()
                        } else {
                            format!(
                                "git reports {} changed file(s), none matching",
                                actual_files.len()
                            )
                        },
                        action: "Open the file. If the work is not there, the claim was wrong."
                            .into(),
                        files: vec![claimed.clone()],
                    });
                }
            }
        }

        // Scope creep: changed but never mentioned.
        let unexplained: Vec<String> = actual_files
            .iter()
            .filter(|a| !claimed_files.iter().any(|c| claim::same_file(c, a)))
            .cloned()
            .collect();
        if !unexplained.is_empty() {
            let shown: Vec<String> = unexplained.iter().take(6).cloned().collect();
            let more = unexplained.len().saturating_sub(shown.len());
            let deleted: Vec<String> = facts
                .changed_since_baseline
                .iter()
                .chain(facts.dirty.iter())
                .filter(|f| {
                    f.status == FileStatus::Deleted && unexplained.iter().any(|u| &f.path == u)
                })
                .map(|f| f.path.clone())
                .collect();
            let mut action = "Review each of these before merging. Work the agent did not tell you about is where surprises live.".to_owned();
            if !deleted.is_empty() {
                action.push_str(&format!(" Note: {} file(s) were DELETED.", deleted.len()));
            }
            findings.push(Finding {
                id: "unmentioned-changes",
                severity: if deleted.is_empty() {
                    Severity::High
                } else {
                    Severity::Critical
                },
                title: format!(
                    "{} file(s) changed that the final message never mentioned.",
                    unexplained.len()
                ),
                claimed: Some(claim_headline(session)),
                evidence: {
                    let mut s = shown.join(", ");
                    if more > 0 {
                        s.push_str(&format!(", ... and {more} more"));
                    }
                    s
                },
                action,
                files: unexplained,
            });
        }
    } else {
        for claimed in claimed_files {
            unchecked.push(claimed.clone());
        }
    }

    // ---- 6. Claimed success with no verification. ----
    //
    // Guarded hard. If any shell action exists whose command we could not read,
    // we do NOT accuse the agent of skipping its tests — we admit we could not see
    // what ran. A false accusation of a lying agent destroys trust in the whole
    // report, so an unreadable command downgrades this to UNKNOWN.
    let unreadable_commands = session
        .actions
        .iter()
        .filter(|a| a.kind == ActionKind::Shell && a.command.is_none())
        .count();
    if verification.is_none() && !session.actions.is_empty() {
        let (sev, title, action) = if unreadable_commands > 0 {
            (
                Severity::Unknown,
                format!(
                    "No verification command was recognised, and {} shell command(s) could not be read.",
                    unreadable_commands
                ),
                "Re-run the tests yourself. acv could not read every command, so it cannot tell you \
                 the agent skipped them."
                    .to_owned(),
            )
        } else {
            (
                Severity::Medium,
                "The session reports success but never ran a test, build, or lint command."
                    .to_owned(),
                "Run the tests yourself. An unverified claim is not a result.".to_owned(),
            )
        };
        findings.push(Finding {
            id: "no-verification",
            severity: sev,
            title,
            claimed: Some(claim_headline(session)),
            evidence: format!(
                "{} action(s) recorded; {} readable, {} unreadable",
                session.actions.len(),
                session
                    .actions
                    .iter()
                    .filter(|a| a.command.is_some())
                    .count(),
                unreadable_commands
            ),
            action,
            files: vec![],
        });
    }

    // ---- 7. We could not check. Always last, always present. ----
    let mut reasons: Vec<String> = Vec::new();
    if !facts.available {
        reasons.push(
            facts
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "git unavailable".to_owned()),
        );
    } else if session.baseline_commit.is_none() {
        reasons.push(
            "session recorded no baseline commit, so per-session attribution is approximate".into(),
        );
    }
    if session.parse.records_unknown_type > 0 {
        reasons.push(format!(
            "{} of {} transcript records were of a type this tool does not model",
            session.parse.records_unknown_type, session.parse.records_seen
        ));
    }
    if session.parse.outcomes_unknown > 0 {
        reasons.push(format!(
            "{} action(s) had no recoverable exit code",
            session.parse.outcomes_unknown
        ));
    }
    if !reasons.is_empty() {
        findings.push(Finding {
            id: "inconclusive",
            severity: Severity::Unknown,
            title: "Part of this session could not be verified. Absence of findings here is NOT a clean bill of health.".into(),
            claimed: None,
            evidence: reasons.join("; "),
            action: "Treat this session as unchecked. Manually confirm the claim, or improve the parser.".into(),
            files: vec![],
        });
    }

    // ---- status rollup ----
    if status != Status::NoWork {
        let has_problems = findings.iter().any(|f| {
            matches!(
                f.severity,
                Severity::Critical | Severity::High | Severity::Medium
            )
        });
        let has_inconclusive = findings.iter().any(|f| f.severity == Severity::Unknown);
        status = match (
            has_problems,
            has_inconclusive,
            verification.is_some(),
            session.aborted,
        ) {
            // An aborted turn is reported as aborted even when we found problems,
            // because "the agent lied" is the wrong frame when a human pressed
            // ESC. The findings still list what changed.
            (_, _, _, true) => Status::Aborted,
            (true, _, _, false) => Status::Discrepancies,
            (false, true, _, false) => Status::Inconclusive,
            (false, false, true, false) => Status::Verified,
            (false, false, false, false) => Status::Unverified,
        };
    }

    // Highest severity first. sort_by_key with Reverse, because CI's clippy is
    // newer than the local toolchain and rejects sort_by on a reversed cmp.
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));

    let verification_commands = format_verification(verification);

    Verdict {
        status,
        findings,
        verification_commands,
        coverage: session.parse.coverage(),
        actual_files,
        unchecked_claim_files: unchecked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::ActionKind;

    fn act(seq: u32, cmd: &str, files: &[&str], outcome: Outcome) -> AgentAction {
        AgentAction {
            schema_version: SCHEMA_VERSION,
            session_id: "s".into(),
            seq,
            ts: None,
            kind: ActionKind::Shell,
            tool: "shell_command".into(),
            command: Some(cmd.into()),
            files: files.iter().map(|s| s.to_string()).collect(),
            outcome,
            exit_code: match outcome {
                Outcome::Success => Some(0),
                Outcome::Failure => Some(1),
                Outcome::Unknown => None,
            },
            workdir: None,
            call_id: None,
        }
    }

    fn base_session() -> Session {
        let mut s = Session::new(AgentKind::Codex, "s1", "test");
        s.parse.records_seen = 10;
        s.parse.records_parsed = 10;
        s.baseline_commit = Some("abc".into());
        s.claim = Claim {
            text: "Fixed the bug. All tests pass.".into(),
            asserted_files: vec![],
            missing: false,
        };
        s
    }

    fn facts_with(files: &[&str]) -> GitFacts {
        GitFacts {
            available: true,
            head: Some("def".into()),
            changed_since_baseline: files
                .iter()
                .map(|f| crate::git::ChangedFile {
                    path: f.to_string(),
                    status: FileStatus::Modified,
                    added: Some(3),
                    deleted: Some(1),
                })
                .collect(),
            dirty: vec![],
            test_harness_detected: true,
            unavailable_reason: None,
        }
    }

    #[test]
    fn no_work_with_a_claim_is_critical() {
        let s = base_session();
        let v = evaluate(&s, &facts_with(&[]));
        assert_eq!(v.status, Status::NoWork);
        assert_eq!(v.findings[0].severity, Severity::Critical);
    }

    #[test]
    fn session_error_beats_everything() {
        let mut s = base_session();
        s.session_error = Some("authentication_failed".into());
        let v = evaluate(&s, &facts_with(&[]));
        assert_eq!(v.status, Status::SessionFailed);
        assert_eq!(v.findings[0].id, "session-error");
    }

    #[test]
    fn unmentioned_change_is_high_severity() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        let v = evaluate(&s, &facts_with(&["src/secret_refactor.rs"]));
        assert_eq!(v.status, Status::Discrepancies);
        let f = v
            .findings
            .iter()
            .find(|f| f.id == "unmentioned-changes")
            .unwrap();
        assert_eq!(f.severity, Severity::High);
    }

    #[test]
    fn deletion_in_unmentioned_changes_escalates_to_critical() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        let mut facts = facts_with(&["src/gone.rs"]);
        facts.changed_since_baseline[0].status = FileStatus::Deleted;
        let v = evaluate(&s, &facts);
        let f = v
            .findings
            .iter()
            .find(|f| f.id == "unmentioned-changes")
            .unwrap();
        assert_eq!(f.severity, Severity::Critical);
    }

    #[test]
    fn success_claim_without_tests_is_unverified_not_verified() {
        let mut s = base_session();
        s.actions
            .push(act(0, "cat src/a.rs", &[], Outcome::Success));
        s.claim.asserted_files = vec!["src/a.rs".into()];
        let v = evaluate(&s, &facts_with(&["src/a.rs"]));
        assert!(v.findings.iter().any(|f| f.id == "no-verification"));
        assert_ne!(v.status, Status::Verified);
    }

    #[test]
    fn clean_session_with_tests_is_verified() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        s.claim.asserted_files = vec!["src/a.rs".into()];
        let v = evaluate(&s, &facts_with(&["src/a.rs"]));
        assert_eq!(
            v.status,
            Status::Verified,
            "findings: {:?}",
            v.findings.iter().map(|f| f.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn claimed_but_untouched_file_is_flagged() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        s.claim.asserted_files = vec!["src/auth/handler.rs".into()];
        let v = evaluate(&s, &facts_with(&["src/other.rs"]));
        assert!(v.findings.iter().any(|f| f.id == "claimed-untouched"));
    }

    #[test]
    fn no_git_is_inconclusive_not_clean() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        let facts = GitFacts {
            unavailable_reason: Some("git not found".into()),
            ..Default::default()
        };
        let v = evaluate(&s, &facts);
        assert!(v.findings.iter().any(|f| f.id == "inconclusive"));
        assert_ne!(v.status, Status::Verified);
    }

    #[test]
    fn verification_detection() {
        assert!(looks_like_verification("npm test"));
        assert!(looks_like_verification("cargo test --all"));
        assert!(looks_like_verification("npx tsc --noEmit"));
        assert!(!looks_like_verification("cat README.md"));
    }

    #[test]
    fn non_zero_exit_is_high() {
        let mut s = base_session();
        s.actions.push(act(0, "npm test", &[], Outcome::Failure));
        s.claim.asserted_files = vec!["a.rs".into()];
        let v = evaluate(&s, &facts_with(&["a.rs"]));
        assert!(v.findings.iter().any(|f| f.id == "ended-on-failure"));
    }

    #[test]
    fn aborted_turn_is_not_reported_as_a_lie() {
        // The user pressed ESC. There is no claim, so accusing the agent of
        // "changing files it never mentioned" is a false accusation.
        let mut s = base_session();
        s.claim = Claim::default();
        s.aborted = true;
        s.actions
            .push(act(0, "New-Item server.py", &[], Outcome::Success));
        let v = evaluate(&s, &facts_with(&["server.py", "client.py"]));
        assert_eq!(v.status, Status::Aborted);
        let ids: Vec<&str> = v.findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&"changed-without-claim"), "{ids:?}");
        assert!(
            !ids.contains(&"unmentioned-changes"),
            "false accusation: {ids:?}"
        );
    }

    #[test]
    fn no_claim_never_produces_unmentioned_changes() {
        let mut s = base_session();
        s.claim = Claim::default();
        s.actions.push(act(0, "npm test", &[], Outcome::Success));
        let v = evaluate(&s, &facts_with(&["a.rs", "b.rs"]));
        let ids: Vec<&str> = v.findings.iter().map(|f| f.id).collect();
        assert!(!ids.contains(&"unmentioned-changes"), "{ids:?}");
        assert!(ids.contains(&"changed-without-claim"), "{ids:?}");
    }
}
