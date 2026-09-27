//! CLI entry point. Argument parsing is hand-rolled to keep the dependency
//! tree at two crates; the release promise is a zero-dependency binary.

use acv::report::{self, Paint, Totals};
use acv::{default_claude_dir, default_codex_dir, discover, evaluate, parse_one, scan, Evaluated};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
acv — did the agent tell the truth?

USAGE
  acv [SCAN] [OPTIONS]        scan sessions and report discrepancies
  acv session <PATH>          verify one specific transcript
  acv doctor                  show where acv looks and what it can see
  acv --help

OPTIONS
  --limit N        most recent N sessions (default 25, 0 = all)
  --all            same as --limit 0
  --json           machine-readable output
  --no-git         skip git ground truth; every session becomes INCONCLUSIVE
  --no-color       disable ANSI colour
  --record         append normalised actions to the ledger ($ACV_LEDGER)
  --codex-dir D    override ~/.codex/sessions
  --claude-dir D   override ~/.claude/projects

ENVIRONMENT
  ACV_LEDGER       ledger path (default ~/.acv/records.jsonl)
  ACV_CODEX_DIR    default codex session dir
  ACV_CLAUDE_DIR   default claude projects dir
  NO_COLOR         set to any value to disable colour

The tool is read-only. It never writes inside your repositories.
";

struct Opts {
    cmd: Cmd,
    limit: usize,
    json: bool,
    no_git: bool,
    no_color: bool,
    record: bool,
    codex_dir: Option<PathBuf>,
    claude_dir: Option<PathBuf>,
    target: Option<PathBuf>,
}

#[derive(PartialEq)]
enum Cmd {
    Scan,
    Session,
    Doctor,
    Help,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return run(default_opts());
    }
    match parse_args(&args) {
        Ok(o) => run(o),
        Err(e) => {
            eprintln!("acv: {e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn default_opts() -> Opts {
    Opts {
        cmd: Cmd::Scan,
        limit: 25,
        json: false,
        no_git: false,
        no_color: false,
        record: false,
        codex_dir: None,
        claude_dir: None,
        target: None,
    }
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = default_opts();
    let mut i = 0;
    if let Some(first) = args.first() {
        match first.as_str() {
            "scan" => {
                o.cmd = Cmd::Scan;
                i = 1;
            }
            "session" => {
                o.cmd = Cmd::Session;
                i = 1;
                if let Some(p) = args.get(1) {
                    o.target = Some(PathBuf::from(p));
                    i = 2;
                }
            }
            "doctor" => {
                o.cmd = Cmd::Doctor;
                i = 1;
            }
            "help" | "-h" | "--help" => {
                o.cmd = Cmd::Help;
                i = args.len();
            }
            _ => {}
        }
    }
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--json" => o.json = true,
            "--no-git" => o.no_git = true,
            "--no-color" => o.no_color = true,
            "--record" => o.record = true,
            "--all" => o.limit = 0,
            "--limit" => {
                i += 1;
                let v = args.get(i).ok_or("--limit needs a number")?;
                o.limit = v.parse().map_err(|_| format!("bad --limit value: {v}"))?;
            }
            "--codex-dir" => {
                i += 1;
                o.codex_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--codex-dir needs a path")?,
                ));
            }
            "--claude-dir" => {
                i += 1;
                o.claude_dir = Some(PathBuf::from(
                    args.get(i).ok_or("--claude-dir needs a path")?,
                ));
            }
            "-h" | "--help" => o.cmd = Cmd::Help,
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(o)
}

fn run(o: Opts) -> ExitCode {
    let paint = Paint::from_env(o.no_color);
    match o.cmd {
        Cmd::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Cmd::Doctor => doctor(&paint, &o),
        Cmd::Session => session_cmd(&paint, &o),
        Cmd::Scan => scan_cmd(&paint, &o),
    }
}

fn doctor(paint: &Paint, o: &Opts) -> ExitCode {
    if o.json {
        let codex = o.codex_dir.clone().or_else(default_codex_dir);
        let claude = o.claude_dir.clone().or_else(default_claude_dir);
        let v = serde_json::json!({
            "codex_dir": codex,
            "claude_dir": claude,
            "codex_found": codex.as_deref().map(|p| discover(p).len()),
            "claude_found": claude.as_deref().map(|p| discover(p).len()),
            "ledger": acv::record::ledger_path(),
            "git_on_path": which_git(),
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    println!("{}", paint.s(report::BOLD, "acv doctor"));
    println!("{}", paint.s(report::DIM, &"—".repeat(78)));
    for (label, dir) in [
        ("codex", o.codex_dir.clone().or_else(default_codex_dir)),
        ("claude", o.claude_dir.clone().or_else(default_claude_dir)),
    ] {
        match dir {
            Some(d) => {
                let n = discover(&d).len();
                let mark = if n > 0 {
                    paint.s(report::GREEN, "ok")
                } else {
                    paint.s(report::YELLOW, "empty")
                };
                println!(
                    "  {label:8} {n:5} transcript(s)  {}",
                    paint.s(report::DIM, &d.display().to_string())
                );
                if n == 0 {
                    println!("           {mark}  nothing found here");
                }
            }
            None => println!(
                "  {label:8}   none  {}",
                paint.s(report::YELLOW, "home directory not resolvable")
            ),
        }
    }
    println!(
        "  {:8} {:5}  {}",
        "git",
        if which_git().is_some() {
            "ok"
        } else {
            "missing"
        },
        which_git().unwrap_or_else(|| "not on PATH; all sessions will be INCONCLUSIVE".into())
    );
    match acv::record::ledger_path() {
        Some(p) => println!("  {:8} {:5}  {}", "ledger", "-", p.display()),
        None => println!("  {:8} {:5}  no writable home; --record will be a no-op", "ledger", "-"),
    }
    ExitCode::SUCCESS
}

fn which_git() -> Option<String> {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn session_cmd(paint: &Paint, o: &Opts) -> ExitCode {
    let Some(target) = o.target.clone() else {
        eprintln!("acv session: needs a transcript path");
        return ExitCode::from(2);
    };
    if !target.exists() {
        eprintln!("acv session: no such file: {}", target.display());
        return ExitCode::from(2);
    }
    let session = match parse_one(&target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("acv session: {e}");
            return ExitCode::from(1);
        }
    };
    let ev = evaluate(&session, o.no_git);
    if o.record {
        record(std::slice::from_ref(&ev.session));
    }
    if o.json {
        println!(
            "{}",
            report::json_report(&[(ev.session.clone(), ev.verdict.clone(), ev.facts.clone())])
        );
    } else {
        print!(
            "{}",
            report::session_block(paint, &ev.session, &ev.verdict, &ev.facts)
        );
    }
    exit_for(&ev)
}

fn scan_cmd(paint: &Paint, o: &Opts) -> ExitCode {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(d) = o.codex_dir.clone().or_else(default_codex_dir) {
        paths.extend(discover(&d));
    }
    if let Some(d) = o.claude_dir.clone().or_else(default_claude_dir) {
        paths.extend(discover(&d));
    }
    if paths.is_empty() {
        eprintln!("acv: no transcripts found. Run `acv doctor` to see where it looked.");
        return ExitCode::from(1);
    }
    let (results, errs) = scan(&paths, o.limit, o.no_git);
    if o.record {
        record(
            &results
                .iter()
                .map(|r| r.session.clone())
                .collect::<Vec<_>>(),
        );
    }

    if o.json {
        let blocks: Vec<_> = results
            .iter()
            .map(|r| (r.session.clone(), r.verdict.clone(), r.facts.clone()))
            .collect();
        println!("{}", report::json_report(&blocks));
    } else {
        let mut totals = Totals::default();
        for r in &results {
            totals.sessions += 1;
            totals.records += r.session.parse.records_seen;
            match r.session.agent {
                acv::record::AgentKind::Codex => totals.codex += 1,
                acv::record::AgentKind::ClaudeCode => totals.claude += 1,
                acv::record::AgentKind::Unknown => {}
            }
            totals.add(&r.verdict);
        }
        totals.coverage = if totals.records == 0 {
            0.0
        } else {
            // Records we actually interpreted, not records that merely parsed as JSON.
            let understood: u64 = results
                .iter()
                .map(|r| {
                    r.session
                        .parse
                        .records_seen
                        .saturating_sub(r.session.parse.records_unknown_type)
                        .saturating_sub(r.session.parse.records_malformed)
                })
                .sum();
            understood as f64 / totals.records as f64
        };
        let blocks: Vec<_> = results
            .iter()
            .map(|r| (r.session.clone(), r.verdict.clone(), r.facts.clone()))
            .collect();
        print!("{}", report::render(paint, &blocks, &totals));
        for e in &errs {
            eprintln!("{} {e}", paint.s(report::DIM, "warning:"));
        }
    }

    let critical = results
        .iter()
        .filter(|r| r.verdict.critical_count() > 0)
        .count();
    if critical > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn exit_for(ev: &Evaluated) -> ExitCode {
    use acv::verdict::Status;
    match ev.verdict.status {
        Status::Discrepancies | Status::NoWork | Status::SessionFailed => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    }
}

fn record(sessions: &[acv::record::Session]) {
    use acv::record::AgentAction;
    let mut lines: Vec<String> = Vec::new();
    for s in sessions {
        for a in &s.actions {
            if let Ok(j) = serde_json::to_string(&AgentAction {
                schema_version: a.schema_version,
                ..a.clone()
            }) {
                lines.push(j);
            }
        }
        if s.actions.is_empty() {
            // Preserve the negative: a session that did nothing is evidence too.
            lines.push(
                serde_json::to_string(&serde_json::json!({
                    "schema_version": acv::record::SCHEMA_VERSION,
                    "session_id": s.session_id,
                    "seq": -1,
                    "kind": "no_action",
                    "tool": "",
                    "outcome": if s.session_error.is_some() { "failure" } else { "unknown" },
                    "session_error": s.session_error,
                }))
                .unwrap_or_default(),
            );
        }
    }
    match acv::record::append_ledger(&lines) {
        Ok(Some(p)) => eprintln!("acv: appended {} record(s) to {}", lines.len(), p.display()),
        Ok(None) => eprintln!("acv: no ledger path available; --record skipped"),
        Err(e) => eprintln!("acv: could not write ledger: {e}"),
    }
}
