//! Git ground truth.
//!
//! Shells out to `git` rather than binding libgit2. Two reasons: the binary
//! stays small, and a missing git is a condition we can detect and report rather
//! than a link error. Every function degrades to `None`/`empty` rather than
//! guessing, because a wrong file list is worse than no file list.

use std::path::Path;
use std::process::Command;

/// A single changed file and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChangedFile {
    /// Path relative to the repo root, forward slashes.
    pub path: String,
    pub status: FileStatus,
    /// Additions/deletions when the diff provides them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Unknown,
}

impl FileStatus {
    pub fn label(&self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Unknown => "changed",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GitFacts {
    /// True when a git repo was found and queried successfully.
    pub available: bool,
    /// Why git could not be used, for honest reporting.
    pub unavailable_reason: Option<String>,
    /// HEAD at time of query.
    pub head: Option<String>,
    /// Files changed relative to the session baseline.
    pub changed_since_baseline: Vec<ChangedFile>,
    /// Files with uncommitted modifications right now.
    pub dirty: Vec<ChangedFile>,
    /// True when tests were found in the repo. We never run them; we only report
    /// whether the *possibility* of verification exists.
    pub test_harness_detected: bool,
}

fn git(dir: &Path, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// A path we refuse to probe.
///
/// UNC and other remote mounts (WSL shares, SMB, Docker volumes, NFS) can block
/// for many seconds, or forever, when the far side is down. Measured on this
/// machine: a single `\\wsl.localhost\...` existence probe took 2.95s with WSL
/// stopped. Across a 60-session scan that is minutes of silence.
///
/// A verifier that hangs is worse than one that says "I could not check this", so
/// remote paths are detected from the string and never touched on disk.
pub fn is_remote(path: &str) -> bool {
    let p = path.replace('/', "\\");
    p.starts_with("\\\\") || p.starts_with("//")
}

pub fn remote_reason(path: &str) -> String {
    format!(
        "session cwd is a remote/UNC path ({path}); not inspected, because remote mounts can block indefinitely"
    )
}

/// Collect ground-truth facts for a session.
///
/// `baseline` is the commit the session started from. When present, we diff
/// against it, which is exact per-session attribution. When absent we fall back
/// to the uncommitted working tree and say so, because that view is *not* the
/// session's changes and conflating them would be a lie.
pub fn collect(dir: &Path, baseline: Option<&str>) -> GitFacts {
    let mut f = GitFacts::default();

    // Remote paths first: never probe them, never shell out to them.
    if let Some(s) = dir.to_str() {
        if is_remote(s) {
            f.unavailable_reason = Some(remote_reason(s));
            return f;
        }
    }

    if !dir.exists() {
        f.unavailable_reason = Some(format!("session cwd no longer exists: {}", dir.display()));
        return f;
    }

    match git(dir, &["rev-parse", "--is-inside-work-tree"]) {
        Some((true, out)) if out.trim() == "true" => {}
        Some((_, out)) => {
            f.unavailable_reason = Some(format!("not a git repository ({})", out.trim()));
            return f;
        }
        None => {
            f.unavailable_reason =
                Some("git executable not found on PATH; file attribution unavailable".to_owned());
            return f;
        }
    }

    f.available = true;
    f.head = git(dir, &["rev-parse", "--short", "HEAD"]).and_then(|(ok, s)| {
        if ok {
            Some(s.trim().to_owned())
        } else {
            None
        }
    });

    match baseline {
        Some(base) => {
            let base = base.trim();
            let exists = {
                let spec = format!("{base}^{{commit}}");
                git(dir, &["cat-file", "-e", &spec])
                    .map(|(ok, _)| ok)
                    .unwrap_or(false)
            };
            if exists {
                f.changed_since_baseline = numstat(dir, &format!("{base}..HEAD"));
                if f.changed_since_baseline.is_empty() {
                    // Nothing committed since baseline: fall back to the working
                    // tree, which is where an uncommitted agent's work lives.
                    f.dirty = numstat(dir, "");
                }
            } else {
                f.unavailable_reason =
                    Some(format!("baseline commit {base} not present in this repo"));
                f.dirty = numstat(dir, "");
            }
        }
        None => {
            f.dirty = numstat(dir, "");
        }
    }

    f.test_harness_detected = detect_tests(dir);
    f
}

/// Changed files for a range, with accurate status.
///
/// Two calls on purpose. `--numstat` gives line counts but no status, so deriving
/// status from it reported every committed deletion as "modified" — which is
/// precisely the case the tool exists to escalate. `--name-status` carries the
/// real code, and porcelain covers untracked files that diff cannot see.
fn numstat(dir: &Path, range: &str) -> Vec<ChangedFile> {
    let range = if range.is_empty() {
        "HEAD".to_owned()
    } else {
        range.to_owned()
    };

    // path -> (added, deleted)
    let mut counts: std::collections::HashMap<String, (Option<u32>, Option<u32>)> =
        std::collections::HashMap::new();
    if let Some((true, out)) = git(dir, &["diff", "--numstat", "--find-renames", &range]) {
        for line in out.lines() {
            let mut it = line.splitn(3, '\t');
            let (Some(a), Some(d), Some(p)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            if p.trim().is_empty() {
                continue;
            }
            counts.insert(
                normalise_path(p.trim()),
                (a.trim().parse().ok(), d.trim().parse().ok()),
            );
        }
    }

    // path -> status
    let mut statuses: std::collections::HashMap<String, FileStatus> =
        std::collections::HashMap::new();
    if let Some((true, out)) = git(dir, &["diff", "--name-status", "--find-renames", &range]) {
        for line in out.lines() {
            let mut it = line.splitn(2, '\t');
            let (Some(code), Some(p)) = (it.next(), it.next()) else {
                continue;
            };
            let code = code.trim();
            let path = normalise_path(p.trim());
            if path.is_empty() {
                continue;
            }
            statuses.insert(
                path,
                match code.chars().next().unwrap_or('M') {
                    'A' => FileStatus::Added,
                    'D' => FileStatus::Deleted,
                    'R' | 'C' => FileStatus::Renamed,
                    _ => FileStatus::Modified,
                },
            );
        }
    }

    // Untracked / staged-but-uncommitted states only show in porcelain.
    for (path, st) in porcelain_status(dir) {
        statuses.entry(path).or_insert(st);
    }

    let mut files: Vec<ChangedFile> = counts
        .into_iter()
        .map(|(path, (added, deleted))| {
            let status = statuses.get(&path).copied().unwrap_or(FileStatus::Modified);
            ChangedFile {
                path,
                status,
                added,
                deleted,
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    files
}

/// `git status --porcelain` parsed once into path -> status.
fn porcelain_status(dir: &Path) -> std::collections::HashMap<String, FileStatus> {
    let mut map = std::collections::HashMap::new();
    let Some((ok, out)) = git(dir, &["status", "--porcelain"]) else {
        return map;
    };
    if !ok {
        return map;
    }
    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        // "XY path" where the path may be quoted.
        let code = line.as_bytes()[0];
        let rest = line[3..].trim();
        // Renames appear as "old -> new".
        let path = normalise_path(rest.rsplit(" -> ").next().unwrap_or(rest));
        let st = match code {
            b'A' | b'?' => FileStatus::Added,
            b'D' => FileStatus::Deleted,
            b'R' => FileStatus::Renamed,
            b'M' => FileStatus::Modified,
            _ => FileStatus::Unknown,
        };
        map.insert(path, st);
    }
    map
}

/// `"a/{old => new}/b"` and `"old => new"` both become forward-slash paths.
fn normalise_path(p: &str) -> String {
    let p = p.trim().trim_matches('"');
    if let Some(start) = p.find('{') {
        if let Some(end) = p.find('}') {
            let inner = &p[start + 1..end];
            if let Some((a, b)) = inner.split_once(" => ") {
                let prefix = &p[..start];
                let suffix = &p[end + 1..];
                let chosen = if b.is_empty() { a } else { b };
                return format!("{prefix}{chosen}{suffix}").replace('\\', "/");
            }
        }
    }
    if p.contains(" => ") {
        return p.rsplit(" => ").next().unwrap_or(p).trim().to_owned();
    }
    p.replace('\\', "/")
}

fn detect_tests(dir: &Path) -> bool {
    const MARKERS: [&str; 12] = [
        "package.json",
        "pytest.ini",
        "pyproject.toml",
        "Cargo.toml",
        "go.mod",
        "Makefile",
        "justfile",
        "tox.ini",
        "pom.xml",
        "build.gradle",
        "vitest.config",
        "jest.config",
    ];
    MARKERS.iter().any(|m| dir.join(m).exists())
        || dir.join("tests").is_dir()
        || dir.join("__tests__").is_dir()
        || dir.join("spec").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_brace_rename() {
        assert_eq!(normalise_path("a/{old => new}/b.rs"), "a/new/b.rs");
    }

    #[test]
    fn normalises_plain_rename() {
        assert_eq!(normalise_path("old.rs => new.rs"), "new.rs");
    }

    #[test]
    fn normalises_windows_separators() {
        assert_eq!(normalise_path("src\\a\\b.rs"), "src/a/b.rs");
    }

    #[test]
    fn missing_dir_is_reported_not_panicked() {
        let f = collect(Path::new("Z:/definitely/not/here"), None);
        assert!(!f.available);
        assert!(f.unavailable_reason.is_some());
    }

    #[test]
    fn remote_paths_are_detected_from_the_string() {
        assert!(is_remote(r"\\wsl.localhost\Ubuntu\home\me\proj"));
        assert!(is_remote(r"\\wsl$\Ubuntu\home\me\proj"));
        assert!(is_remote("//server/share/proj"));
        assert!(!is_remote(r"C:\Users\me\proj"));
        assert!(!is_remote("/home/me/proj"));
        assert!(!is_remote("C:/Users/me/proj"));
    }

    #[test]
    fn remote_path_is_never_probed() {
        // Must return immediately with a reason, not attempt any filesystem or
        // git access against the UNC path.
        let f = collect(Path::new(r"\\wsl.localhost\Ubuntu\home\me\proj"), None);
        assert!(!f.available);
        let r = f.unavailable_reason.unwrap();
        assert!(r.contains("remote"), "reason was: {r}");
    }
}
