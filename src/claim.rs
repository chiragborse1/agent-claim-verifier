//! Claim analysis: what did the agent *say* it did?
//!
//! This is deliberately a heuristic, and the heuristic is deliberately
//! conservative. `extract_paths` returns paths the text literally names. It does
//! not guess, does not fuzzy-match, and does not resolve names against the repo.
//! A missed path costs us one extra discrepancy; an invented path costs the user
//! their trust in the whole report.

/// Extract file-path-looking tokens from free text.
///
/// Handles Windows (`C:\a\b.js`, `a\b.js`) and POSIX (`a/b.js`, `./a/b.js`)
/// separators, plus bare filenames with a known-ish extension. Requires a dot
/// extension so ordinary sentence-ending periods do not produce noise.
pub fn extract_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace()
            || c == '"'
            || c == '\''
            || c == '`'
            || c == '('
            || c == ')'
            || c == '['
            || c == ']'
    }) {
        // Trim sentence punctuation from both ends, then normalise away a leading
        // "./" or "/" so the result is repo-root-relative like git reports it.
        let mut tok = raw
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '.' | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | '*'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '"'
                        | '\''
                        | '`'
                )
            })
            .to_owned();
        loop {
            let before = tok.len();
            if tok.starts_with("./") {
                tok.remove(0);
                tok.remove(0);
            }
            while tok.starts_with('/') || tok.starts_with('\\') {
                tok.remove(0);
            }
            if tok.len() == before {
                break;
            }
        }
        let tok = tok.as_str();
        if tok.len() < 3 || tok.len() > 300 {
            continue;
        }
        if !has_extension(tok) {
            continue;
        }
        // Must contain a separator or be a bare filename. Reject things that are
        // clearly prose containing a dot, e.g. "e.g.config".
        let has_sep = tok.contains('/') || tok.contains('\\');
        let drive = tok.len() > 2 && tok.as_bytes()[1] == b':';
        if !has_sep && !drive {
            // Bare filename like `main.js` is legitimate evidence. Keep it.
            if tok.contains(' ') {
                continue;
            }
        }
        if tok.ends_with('.') {
            continue;
        }
        out.push(tok.to_owned());
    }
    out.sort();
    out.dedup();
    out
}

fn has_extension(tok: &str) -> bool {
    let Some(dot) = tok.rfind('.') else {
        return false;
    };
    if dot == 0 || dot + 1 >= tok.len() {
        return false;
    }
    let ext = &tok[dot + 1..];
    // Extension must be short and alphanumeric: js, ts, py, go, rs, json, md, yml.
    ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Reduce a path to a comparable suffix so a claim saying `axon-app/main.js`
/// matches git reporting `openclaw-main/axon-app/main.js`.
///
/// Returns the tail of the path with a fixed depth. Deliberately crude and
/// symmetric: it is used only to decide whether two mentions plausibly refer to
/// the same file, and every match it produces is shown to the user.
pub fn path_tail(path: &str, depth: usize) -> String {
    let norm = path.replace('\\', "/");
    let parts: Vec<&str> = norm
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.len() <= depth {
        return norm.to_ascii_lowercase();
    }
    parts[parts.len() - depth..].join("/").to_ascii_lowercase()
}

/// Do two path mentions plausibly refer to the same file?
pub fn same_file(a: &str, b: &str) -> bool {
    let na = a.replace('\\', "/").to_ascii_lowercase();
    let nb = b.replace('\\', "/").to_ascii_lowercase();
    if na == nb {
        return true;
    }
    if na.ends_with(&nb) || nb.ends_with(&na) {
        return true;
    }
    path_tail(&na, 2) == path_tail(&nb, 2)
}

/// Does a claim read as an assertion of completion?
///
/// Used only to prioritise which sentence to show the user, never to decide
/// whether work happened. Presence of a final-phase message is the signal;
/// this is presentation.
pub fn reads_as_completion(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    const MARKERS: [&str; 12] = [
        "done",
        "complete",
        "finished",
        "fixed",
        "implemented",
        "added",
        "all tests",
        "tests pass",
        "passing",
        "resolved",
        "verified",
        "ready",
    ];
    MARKERS.iter().any(|m| t.contains(m))
}

/// First sentence-ish chunk of a claim, for display. Bounded so a 4,000-word
/// final message cannot blow out a terminal report.
pub fn summarise(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_absolute_and_relative_paths() {
        let got = extract_paths("I updated C:\\repo\\src\\main.rs and src/lib.rs today.");
        assert!(got.contains(&"C:\\repo\\src\\main.rs".to_string()));
        assert!(got.contains(&"src/lib.rs".to_string()));
    }

    #[test]
    fn finds_posix_paths() {
        let got = extract_paths("see ./openclaw-main/axon-app/main.js for details");
        // The leading "./" is stripped deliberately: git reports repo-root-relative
        // paths, so normalising here is what makes the two comparable.
        assert!(
            got.contains(&"openclaw-main/axon-app/main.js".to_string()),
            "got {got:?}"
        );
    }

    #[test]
    fn ignores_prose_and_sentence_periods() {
        let got = extract_paths("The build failed. I fixed it. Tests pass now.");
        assert!(got.is_empty(), "got {got:?}");
    }

    #[test]
    fn ignores_extensionless_tokens() {
        let got = extract_paths("update the Makefile and README next");
        assert!(got.is_empty(), "got {got:?}");
    }

    #[test]
    fn same_file_matches_on_tail() {
        assert!(same_file(
            "openclaw-main/axon-app/main.js",
            "axon-app/main.js"
        ));
        assert!(same_file("C:\\x\\y\\main.js", "main.js"));
        assert!(!same_file("src/auth.rs", "src/billing.rs"));
    }

    #[test]
    fn completion_detection() {
        assert!(reads_as_completion("All tests pass, the fix is in."));
        assert!(!reads_as_completion("I am now tracing the registry path."));
    }

    #[test]
    fn summarise_truncates() {
        let long = "word ".repeat(200);
        let s = summarise(&long, 40);
        assert!(s.chars().count() <= 43);
        assert!(s.ends_with("..."));
    }
}
