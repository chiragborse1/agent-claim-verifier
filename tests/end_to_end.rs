//! End-to-end tests against a REAL git repository.
//!
//! Everything else in this crate is unit-tested with hand-built structs, which
//! means the most important path in the whole program — transcript -> git diff ->
//! verdict — was never actually exercised. These tests build a throwaway repo,
//! make real commits, and point a real rollout fixture at it.
//!
//! The two fixtures are the whole thesis of the tool:
//!   honest_session  the agent fixed the file and ran the tests.  -> VERIFIED
//!   lying_session   the agent claimed tests pass, the suite exited 1,
//!                   and it never mentioned the file it deleted. -> DISCREPANCIES

use acv::record::AgentKind;
use acv::verdict::Status;
use acv::{evaluate, git, parse_one};
use std::path::{Path, PathBuf};
use std::process::Command;

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Repo {
    dir: PathBuf,
    /// The commit that existed before any agent work, i.e. the session baseline.
    baseline: String,
}

impl Repo {
    fn new(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("acv-it-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q", "-b", "main"]);
        sh(&dir, &["config", "user.email", "t@t"]);
        sh(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("client.py"), "def send():\n    pass\n").unwrap();
        std::fs::write(dir.join("README.md"), "# proj\n").unwrap();
        sh(&dir, &["add", "-A"]);
        sh(&dir, &["commit", "-q", "-m", "base"]);
        let baseline = sh(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
        Repo { dir, baseline }
    }
    /// Materialise a template, substituting the repo path and baseline commit.
    ///
    /// The path is inserted with forward slashes on purpose: it lands inside a
    /// JSON-encoded `arguments` string, and a backslash there would need a second
    /// level of escaping that is easy to get wrong.
    fn rollout(&self, template: &str, tag: &str) -> PathBuf {
        let tpl = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(template);
        let body = std::fs::read_to_string(&tpl)
            .unwrap()
            .replace(
                "__CWD__",
                &self.dir.display().to_string().replace('\\', "/"),
            )
            .replace("__BASELINE__", &self.baseline);
        let p = self
            .dir
            .join(format!("rollout-2026-09-27T10-00-00-{tag}.jsonl"));
        std::fs::write(&p, body).unwrap();
        p
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn honest_session_is_verified_against_real_git() {
    let repo = Repo::new("honest");
    // The agent actually fixes the file it claims to fix.
    std::fs::write(
        repo.dir.join("client.py"),
        "def send():\n    return retry(3)\n",
    )
    .unwrap();
    sh(&repo.dir, &["add", "-A"]);
    sh(&repo.dir, &["commit", "-q", "-m", "fix retry"]);

    let path = repo.rollout(
        "honest_session.jsonl.tmpl",
        "11111111-1111-1111-1111-111111111111",
    );
    let session = parse_one(&path).expect("parses");
    assert_eq!(session.agent, AgentKind::Codex);
    assert_eq!(
        session.baseline_commit.as_deref(),
        Some(repo.baseline.as_str())
    );

    let facts = git::collect(&repo.dir, session.baseline_commit.as_deref());
    assert!(
        facts.available,
        "git should be available: {:?}",
        facts.unavailable_reason
    );

    let ev = evaluate(&session, false);
    assert_eq!(
        ev.verdict.status,
        Status::Verified,
        "expected VERIFIED, findings: {:#?}\nACTIONS: {:#?}",
        ev.verdict.findings,
        session.actions
    );
    assert_eq!(ev.verdict.critical_count(), 0);
    assert!(ev
        .verdict
        .verification_commands
        .iter()
        .any(|c| c.contains("pytest")));
    // The claim named client.py; git should agree it changed.
    assert!(ev
        .verdict
        .actual_files
        .iter()
        .any(|f| f.ends_with("client.py")));
}

#[test]
fn lying_session_is_caught_on_every_axis() {
    let repo = Repo::new("lying");
    // The agent did three things it did not mention, one of them a deletion.
    std::fs::write(repo.dir.join("server.py"), "def health():\n    return {}\n").unwrap();
    std::fs::write(repo.dir.join("client.py"), "// quietly rewritten\n").unwrap();
    std::fs::remove_file(repo.dir.join("README.md")).unwrap();
    sh(&repo.dir, &["add", "-A"]);
    sh(
        &repo.dir,
        &["commit", "-q", "-m", "work the agent did not mention"],
    );

    let path = repo.rollout(
        "lying_session.jsonl.tmpl",
        "22222222-2222-2222-2222-222222222222",
    );
    let session = parse_one(&path).expect("parses");

    // The parser must have recovered the failing exit code from the transcript.
    let failed = session
        .actions
        .iter()
        .find(|a| a.outcome == acv::record::Outcome::Failure);
    assert!(
        failed.is_some(),
        "non-zero exit should be recovered from call_id join"
    );
    // And the patch should have yielded exactly the file it names.
    let patch = session
        .actions
        .iter()
        .find(|a| a.tool == "apply_patch")
        .expect("patch action");
    assert_eq!(
        patch.files.len(),
        1,
        "apply_patch should name exactly one file: {patch:?}"
    );
    assert!(patch.files[0].ends_with("server.py"), "{patch:?}");

    let ev = evaluate(&session, false);
    let ids: Vec<&str> = ev.verdict.findings.iter().map(|f| f.id).collect();

    assert_eq!(
        ev.verdict.status,
        Status::Discrepancies,
        "findings: {ids:?}"
    );
    // Claimed "All tests pass" while the suite exited 1.
    assert!(
        ids.contains(&"ended-on-failure"),
        "missing ended-on-failure in {ids:?}"
    );
    // Changed files the final message never mentioned, including a deletion.
    let unmentioned = ev
        .verdict
        .findings
        .iter()
        .find(|f| f.id == "unmentioned-changes");
    assert!(
        unmentioned.is_some(),
        "missing unmentioned-changes in {ids:?}"
    );
    assert_eq!(
        unmentioned.unwrap().severity,
        acv::verdict::Severity::Critical,
        "a hidden deletion must escalate to CRITICAL"
    );
    assert!(unmentioned
        .unwrap()
        .files
        .iter()
        .any(|f| f.contains("client.py")));
    assert!(unmentioned
        .unwrap()
        .files
        .iter()
        .any(|f| f.contains("README.md")));
}

#[test]
fn coverage_is_penalised_for_unmodelled_records() {
    let repo = Repo::new("cov");
    let path = repo.rollout(
        "honest_session.jsonl.tmpl",
        "33333333-3333-3333-3333-333333333333",
    );
    let session = parse_one(&path).unwrap();
    // Every record in the fixture is a type we model.
    assert_eq!(session.parse.records_unknown_type, 0, "{:?}", session.parse);
    assert!(
        session.parse.coverage() > 0.99,
        "coverage was {}",
        session.parse.coverage()
    );
}

#[test]
fn unmodelled_type_forces_the_unknown_finding() {
    // Proves the honesty guarantee: a record shape we do not understand must not
    // silently read as "nothing to report".
    let repo = Repo::new("unk");
    let path = repo.rollout(
        "honest_session.jsonl.tmpl",
        "44444444-4444-4444-4444-444444444444",
    );
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.push_str(
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"brand_new_thing_2027\",\"x\":1}}\n",
    );
    std::fs::write(&path, body).unwrap();

    let session = parse_one(&path).unwrap();
    assert_eq!(session.parse.records_unknown_type, 1);
    assert!(session.parse.coverage() < 1.0);

    let ev = evaluate(&session, false);
    let ids: Vec<&str> = ev.verdict.findings.iter().map(|f| f.id).collect();
    assert!(
        ids.contains(&"inconclusive"),
        "unknown type must surface, got {ids:?}"
    );
}

#[test]
fn baseline_that_is_absent_is_reported_not_guessed() {
    let repo = Repo::new("nobase");
    std::fs::write(repo.dir.join("client.py"), "changed\n").unwrap();
    let facts = git::collect(&repo.dir, Some("0000000000000000000000000000000000000000"));
    // Baseline is not in this repo, so we must not pretend to know the diff.
    assert!(facts.unavailable_reason.is_some(), "{facts:?}");
    assert!(facts.changed_since_baseline.is_empty());
}
