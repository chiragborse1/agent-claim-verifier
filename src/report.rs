//! Terminal and JSON rendering.
//!
//! Design rule: the report must be impossible to misread as "all clear" when it
//! is not. Session headers carry their status word in plain ASCII, severity
//! words are spelled out, and the coverage line is always printed.

use crate::git::GitFacts;
use crate::record::Session;
use crate::verdict::{Severity, Status, Verdict};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const GREEN: &str = "\x1b[32m";
pub const CYAN: &str = "\x1b[36m";
pub const MAGENTA: &str = "\x1b[35m";

pub struct Paint {
    pub on: bool,
}

impl Paint {
    pub fn from_env(no_color: bool) -> Self {
        Paint {
            on: !no_color && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    pub fn s(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("{code}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }
    pub fn status(&self, s: Status) -> String {
        let c = match s {
            Status::Verified => GREEN,
            Status::Discrepancies => RED,
            Status::Unverified
            | Status::NoWork
            | Status::SessionFailed
            | Status::Inconclusive
            | Status::Aborted => YELLOW,
        };
        self.s(c, s.label())
    }
    pub fn severity(&self, s: Severity) -> String {
        let c = match s {
            Severity::Critical => RED,
            Severity::High => YELLOW,
            Severity::Medium => YELLOW,
            Severity::Low => DIM,
            Severity::Unknown => MAGENTA,
        };
        self.s(c, s.label())
    }
}

const W: usize = 78;

fn rule(c: &Paint) -> String {
    c.s(DIM, &"-".repeat(W))
}

/// One session's findings, rendered as a block.
pub fn session_block(c: &Paint, s: &Session, v: &Verdict, facts: &GitFacts) -> String {
    let mut out = String::new();
    let cwd = s.cwd.clone().unwrap_or_else(|| "(cwd unknown)".into());
    let short_id: String = s.session_id.chars().take(12).collect();
    let loc = cwd.replace('\\', "/");
    let loc_short = if loc.chars().count() > 34 {
        let tail: String = loc.chars().skip(loc.chars().count() - 34).collect();
        format!("...{tail}")
    } else {
        loc.clone()
    };

    out.push_str(&format!(
        "{}  {}  {}\n",
        c.status(v.status),
        c.s(BOLD, &short_id),
        c.s(
            DIM,
            &format!(
                "{} · {} · {}",
                s.agent.label(),
                loc_short,
                s.branch.clone().unwrap_or_else(|| "no branch".into())
            )
        )
    ));
    out.push_str(&format!(
        "{}\n",
        c.s(DIM, &format!("  {}", s.source_path.replace('\\', "/")))
    ));
    if let Some(e) = &s.session_error {
        out.push_str(&format!("  {} {}\n", c.s(RED, "session error:"), e));
    }
    out.push_str(&format!(
        "  {} {}\n",
        c.s(DIM, "claim:"),
        c.s(CYAN, &crate::verdict::claim_headline(s))
    ));
    if let Some(cmd) = v.verification_commands.first() {
        // Never imply a check passed. The exit code is part of the string.
        let ok = !cmd.contains("exit 0");
        let label = if ok { "verification:" } else { "verified by:" };
        let colour = if ok { YELLOW } else { GREEN };
        out.push_str(&format!("  {} {}\n", c.s(colour, label), cmd));
    }
    if !facts.available {
        out.push_str(&format!(
            "  {} {}\n",
            c.s(MAGENTA, "git:"),
            facts
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "unavailable".into())
        ));
    } else if facts.changed_since_baseline.is_empty() && facts.dirty.is_empty() {
        out.push_str(&format!(
            "  {} git reports no file changes\n",
            c.s(DIM, "git:")
        ));
    } else {
        let n = facts.changed_since_baseline.len().max(facts.dirty.len());
        out.push_str(&format!(
            "  {} git reports {n} changed file(s)\n",
            c.s(DIM, "git:")
        ));
    }

    if v.findings.is_empty() {
        out.push_str(&format!("  {}\n", c.s(GREEN, "no discrepancies found")));
        return out;
    }
    for (i, f) in v.findings.iter().enumerate() {
        out.push('\n');
        out.push_str(&format!(
            "  {}. [{}] {}\n",
            i + 1,
            c.severity(f.severity),
            c.s(BOLD, &f.title)
        ));
        if let Some(cl) = &f.claimed {
            out.push_str(&format!(
                "     {} {}\n",
                c.s(DIM, "claimed "),
                crate::claim::summarise(cl, 150)
            ));
        }
        out.push_str(&format!("     {} {}\n", c.s(DIM, "evidence"), f.evidence));
        out.push_str(&format!("     {} {}\n", c.s(DIM, "do     "), f.action));
    }
    out
}

/// Full scan report.
pub fn render(c: &Paint, blocks: &[(Session, Verdict, GitFacts)], totals: &Totals) -> String {
    let mut out = String::new();
    out.push_str(&c.s(BOLD, "acv"));
    out.push_str(&c.s(DIM, "  did the agent tell the truth?\n\n"));
    out.push_str(&format!(
        "{} {} codex · {} claude-code · {} sessions scanned\n\n",
        c.s(DIM, "SCAN "),
        totals.codex,
        totals.claude,
        totals.sessions
    ));
    out.push_str(&rule(c));
    out.push('\n');

    if blocks.is_empty() {
        out.push_str(&format!(
            "{}\n{}\n",
            c.s(YELLOW, "no sessions found"),
            c.s(DIM, "acv doctor will show where it looked")
        ));
        return out;
    }

    for (s, v, f) in blocks {
        out.push_str(&session_block(c, s, v, f));
        out.push_str(&rule(c));
        out.push('\n');
    }

    out.push_str(&c.s(BOLD, "SUMMARY\n"));
    out.push_str(&format!(
        "  {}  {} {} · {} {} · {} {} · {} {}\n",
        c.s(DIM, "severity"),
        c.s(RED, &totals.critical.to_string()),
        c.s(DIM, "CRITICAL"),
        c.s(YELLOW, &totals.high.to_string()),
        c.s(DIM, "HIGH"),
        c.s(YELLOW, &totals.medium.to_string()),
        c.s(DIM, "MEDIUM"),
        c.s(MAGENTA, &totals.unknown.to_string()),
        c.s(DIM, "UNKNOWN"),
    ));
    out.push_str(&format!(
        "  {}  {} · {} · {} · {} · {} · {} · {}\n",
        c.s(DIM, "status   "),
        c.s(GREEN, &format!("{} verified", totals.verified)),
        c.s(RED, &format!("{} discrepancies", totals.discrepancies)),
        c.s(YELLOW, &format!("{} unverified", totals.unverified)),
        c.s(MAGENTA, &format!("{} inconclusive", totals.inconclusive)),
        c.s(YELLOW, &format!("{} no-work", totals.no_work)),
        c.s(RED, &format!("{} failed", totals.failed)),
        c.s(YELLOW, &format!("{} aborted", totals.aborted)),
    ));
    out.push_str(&format!(
        "  {} {:.1}% of {} transcript records modelled\n",
        c.s(DIM, "coverage"),
        totals.coverage * 100.0,
        totals.records
    ));
    if totals.coverage < 0.9 {
        out.push_str(&format!(
            "  {} coverage below 90%: treat the clean sessions as unchecked, not verified\n",
            c.s(MAGENTA, "note:")
        ));
    }
    out
}

#[derive(Default, Clone)]
pub struct Totals {
    pub sessions: usize,
    pub codex: usize,
    pub claude: usize,
    pub records: u64,
    pub coverage: f64,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub unknown: usize,
    pub verified: usize,
    pub discrepancies: usize,
    pub unverified: usize,
    pub inconclusive: usize,
    pub no_work: usize,
    pub failed: usize,
    pub aborted: usize,
}

impl Totals {
    pub fn add(&mut self, v: &Verdict) {
        for f in &v.findings {
            match f.severity {
                Severity::Critical => self.critical += 1,
                Severity::High => self.high += 1,
                Severity::Medium => self.medium += 1,
                Severity::Low => self.low += 1,
                Severity::Unknown => self.unknown += 1,
            }
        }
        match v.status {
            Status::Verified => self.verified += 1,
            Status::Discrepancies => self.discrepancies += 1,
            Status::Unverified => self.unverified += 1,
            Status::Inconclusive => self.inconclusive += 1,
            Status::NoWork => self.no_work += 1,
            Status::SessionFailed => self.failed += 1,
            Status::Aborted => self.aborted += 1,
        }
    }
}

pub fn json_report(blocks: &[(Session, Verdict, GitFacts)]) -> String {
    let items: Vec<serde_json::Value> = blocks
        .iter()
        .map(|(s, v, f)| {
            serde_json::json!({
                "session_id": s.session_id,
                "agent": s.agent.label(),
                "source": s.source_path,
                "cwd": s.cwd,
                "branch": s.branch,
                "baseline_commit": s.baseline_commit,
                "started_at": s.started_at,
                "ended_at": s.ended_at,
                "session_error": s.session_error,
                "claim": {
                    "text": s.claim.text,
                    "asserted_files": s.claim.asserted_files,
                    "missing": s.claim.missing,
                },
                "parse": {
                    "records_seen": s.parse.records_seen,
                    "records_parsed": s.parse.records_parsed,
                    "records_unknown_type": s.parse.records_unknown_type,
                    "records_malformed": s.parse.records_malformed,
                    "outcomes_unknown": s.parse.outcomes_unknown,
                    "coverage": s.parse.coverage(),
                },
                "git": {
                    "available": f.available,
                    "unavailable_reason": f.unavailable_reason,
                    "head": f.head,
                    "changed_since_baseline": f.changed_since_baseline,
                    "dirty": f.dirty,
                    "test_harness_detected": f.test_harness_detected,
                },
                "verdict": {
                    "status": v.status.label(),
                    "coverage": v.coverage,
                    "verification_commands": v.verification_commands,
                    "actual_files": v.actual_files,
                    "unchecked_claim_files": v.unchecked_claim_files,
                    "findings": v.findings.iter().map(|x| serde_json::json!({
                        "id": x.id,
                        "severity": x.severity.label(),
                        "title": x.title,
                        "claimed": x.claimed,
                        "evidence": x.evidence,
                        "action": x.action,
                        "files": x.files,
                    })).collect::<Vec<_>>(),
                },
                "actions": s.actions,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(items))
        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}
