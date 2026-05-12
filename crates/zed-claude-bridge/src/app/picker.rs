//! Native picker for the `ipc-send-at-mention` helper.
//!
//! When the sidecar replies with an `IpcFrame::Ambiguous`, the helper
//! needs to ask the user which Claude session should receive the
//! at-mention. On **macOS** we shell out to `osascript -e 'choose
//! from list …'` synchronously; the helper has no other work to do
//! while the dialog is open. On **other platforms** we deterministically
//! fall back to the most-recently-active candidate (smallest
//! `last_activity_ms_ago`) and log a WARN — adding a native Linux
//! picker (`zenity`/`kdialog`) is a separate OpenSpec follow-up.
//!
//! Layer position: layer 6 (`app/`). This module is allowed to spawn
//! subprocesses and to log; it is the only place in the crate that
//! talks to `osascript`.
//!
//! See `design.md` D4 for the rationale (why the helper, not the
//! daemon, owns the dialog) and the
//! `openspec/changes/session-routing/specs/protocol/spec.md`
//! requirement "ipc-send-at-mention picker round-trip behaviour" for
//! the user-visible contract this module implements.

use uuid::Uuid;

use crate::protocol::AmbiguousCandidate;

/// Prompt shown above the picker on macOS. Single source of truth so
/// the test that asserts the script shape can match against the same
/// literal.
const PICKER_PROMPT: &str = "Send selection to which Claude session?";

/// Pick one candidate from an `Ambiguous` reply.
///
/// Semantics:
/// - On **macOS**, opens a native `choose from list` dialog with one
///   row per candidate's `label`. Returns `Some(candidate.client_id)`
///   on a successful pick, `None` on cancellation, or on any
///   unrecoverable `osascript` failure (after logging at ERROR).
/// - On **non-macOS** platforms, logs a WARN, then returns
///   `Some(client_id)` of the candidate with the smallest
///   `last_activity_ms_ago`. Returns `None` only when `candidates`
///   is empty.
pub fn pick_candidate(candidates: &[AmbiguousCandidate]) -> Option<Uuid> {
    if candidates.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        pick_macos(candidates)
    }
    #[cfg(not(target_os = "macos"))]
    {
        pick_fallback_mra(candidates)
    }
}

/// Most-recently-active fallback used on non-macOS platforms.
///
/// Public-to-the-crate so we can test it directly without needing to
/// run under a non-macOS target; the macOS code path never calls
/// this in production. The `cfg` gate keeps it visible to unit tests
/// on every platform while excluding it from the macOS production
/// build (where it would otherwise be flagged as dead code by
/// clippy).
#[cfg(any(test, not(target_os = "macos")))]
pub(crate) fn pick_fallback_mra(candidates: &[AmbiguousCandidate]) -> Option<Uuid> {
    tracing::warn!(
        "ambiguous workspace match; picker not available on this platform; falling back to most-recently-active candidate"
    );
    candidates
        .iter()
        .min_by_key(|c| c.last_activity_ms_ago)
        .map(|c| c.client_id)
}

/// macOS path: build an AppleScript snippet, invoke `osascript -e
/// <script>`, parse stdout, and map back to a `client_id`.
#[cfg(target_os = "macos")]
fn pick_macos(candidates: &[AmbiguousCandidate]) -> Option<Uuid> {
    use std::process::Command;

    let script = build_applescript(candidates);
    let output = match Command::new("osascript").args(["-e", &script]).output() {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "failed to spawn osascript; cannot present picker");
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!(
            status = ?output.status,
            stderr = %stderr,
            "osascript exited non-zero; cannot present picker"
        );
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_osascript_pick(stdout.as_ref(), candidates)
}

/// Parse `osascript`'s stdout from `choose from list`:
/// - `"<label>\n"` on a successful pick (we trim the trailing newline).
/// - `"false\n"` on cancellation.
///
/// We map the chosen label back to its candidate's `client_id` by
/// linear scan; labels are guaranteed distinct per the router's
/// `disambiguate_labels` helper. Returns `None` on cancel, on an
/// unmatched label (logged at ERROR), or on empty stdout.
fn parse_osascript_pick(stdout: &str, candidates: &[AmbiguousCandidate]) -> Option<Uuid> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "false" {
        // User cancelled the dialog. Intentionally drop the at-mention.
        return None;
    }
    if let Some(c) = candidates.iter().find(|c| c.label == trimmed) {
        return Some(c.client_id);
    }
    tracing::error!(
        chosen = %trimmed,
        "osascript returned a label that did not match any candidate; cannot route"
    );
    None
}

/// Build the AppleScript snippet for `choose from list`.
///
/// Shape:
///
/// ```text
/// set _items to {"<label1>", "<label2>"}
/// choose from list _items with prompt "Send selection to which Claude session?" default items {item 1 of _items}
/// ```
///
/// Each label is escaped via [`applescript_escape`] so embedded `"`
/// and `\` characters do not break out of the string literal.
fn build_applescript(candidates: &[AmbiguousCandidate]) -> String {
    let items: Vec<String> = candidates
        .iter()
        .map(|c| format!("\"{}\"", applescript_escape(&c.label)))
        .collect();
    let items_joined = items.join(", ");
    format!(
        "set _items to {{{items}}}\nchoose from list _items with prompt \"{prompt}\" default items {{item 1 of _items}}",
        items = items_joined,
        prompt = applescript_escape(PICKER_PROMPT),
    )
}

/// Escape a string so it can be embedded inside an AppleScript double-quoted
/// string literal without breaking the parser.
///
/// AppleScript's string literal grammar treats `\` as an escape
/// character and `"` as the literal terminator. Replace `\` first
/// (so we do not double-escape the `\` we just inserted), then `"`.
/// All other characters (including UTF-8 multibyte sequences and the
/// em-dash used in our labels) are passed through verbatim.
pub(crate) fn applescript_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "tests legitimately panic and unwrap on assertion failures"
)]
mod tests {
    use super::*;

    fn candidate(label: &str, last_activity_ms_ago: u64) -> AmbiguousCandidate {
        AmbiguousCandidate {
            client_id: Uuid::new_v4(),
            label: label.to_string(),
            connected_at_ms_ago: 1_000,
            last_activity_ms_ago,
        }
    }

    // ---------- applescript_escape ----------

    #[test]
    fn applescript_escape_is_identity_for_plain_ascii() {
        assert_eq!(applescript_escape("Session 1"), "Session 1");
        assert_eq!(applescript_escape(""), "");
        // Em-dash and other multibyte UTF-8 characters pass through.
        assert_eq!(
            applescript_escape("Session 1 — connected 5s ago"),
            "Session 1 — connected 5s ago"
        );
    }

    #[test]
    fn applescript_escape_escapes_double_quote() {
        assert_eq!(applescript_escape(r#"foo "bar" baz"#), r#"foo \"bar\" baz"#);
    }

    #[test]
    fn applescript_escape_escapes_backslash() {
        // Raw input: foo \ bar
        // Expected:  foo \\ bar  (the backslash doubled)
        assert_eq!(applescript_escape(r"foo \ bar"), r"foo \\ bar");
    }

    #[test]
    fn applescript_escape_does_not_double_escape_doubled_backslash() {
        // Input has a literal backslash followed by a quote. The escape
        // step transforms `\` → `\\` first, then `"` → `\"`. Verify we
        // do NOT then re-escape the freshly inserted backslash.
        assert_eq!(applescript_escape(r#"\"x"#), r#"\\\"x"#);
    }

    // ---------- build_applescript shape ----------

    #[test]
    fn build_applescript_one_candidate_is_syntactically_valid() {
        let c = candidate("Session 1 — connected 5s ago", 100);
        let script = build_applescript(std::slice::from_ref(&c));
        // Brace balance: every opening `{` has a matching `}`.
        assert_eq!(
            script.matches('{').count(),
            script.matches('}').count(),
            "unbalanced braces in script: {script}"
        );
        // The list literal contains the candidate's label (escaped).
        assert!(
            script.contains("Session 1 — connected 5s ago"),
            "label missing: {script}"
        );
        // Literal AppleScript keyword present.
        assert!(
            script.contains("choose from list _items with prompt"),
            "missing 'choose from list' keyword: {script}"
        );
        // Default-item clause present so the dialog opens with a pre-selected row.
        assert!(
            script.contains("default items {item 1 of _items}"),
            "missing default items clause: {script}"
        );
        // The prompt matches our constant.
        assert!(script.contains(PICKER_PROMPT), "prompt missing: {script}");
    }

    #[test]
    fn build_applescript_two_candidates_emits_both_labels() {
        let c1 = candidate("Session 1 — A", 500);
        let c2 = candidate("Session 2 — B", 200);
        let script = build_applescript(&[c1, c2]);
        assert_eq!(
            script.matches('{').count(),
            script.matches('}').count(),
            "unbalanced braces: {script}"
        );
        assert!(script.contains("Session 1 — A"), "first label missing");
        assert!(script.contains("Session 2 — B"), "second label missing");
        // The two labels are separated by a comma (list literal).
        let s1 = script.find("Session 1 — A").expect("first label idx");
        let s2 = script.find("Session 2 — B").expect("second label idx");
        assert!(s1 < s2, "labels appear out of input order in script");
    }

    #[test]
    fn build_applescript_escapes_user_supplied_quotes_in_labels() {
        // A defensive test: even though our own labels never contain `"`,
        // a future label format change must not break the AppleScript.
        let c = candidate(r#"Weird "quoted" session"#, 100);
        let script = build_applescript(&[c]);
        // The escaped sequence `\"quoted\"` MUST appear; an unescaped
        // `"quoted"` inside the list literal would terminate the string
        // literal prematurely.
        assert!(
            script.contains(r#"\"quoted\""#),
            "embedded quotes were not escaped: {script}"
        );
    }

    // ---------- parse_osascript_pick ----------

    #[test]
    fn parse_osascript_pick_returns_uuid_on_label_match() {
        let c1 = candidate("Session 1", 500);
        let c2 = candidate("Session 2", 200);
        let expected = c2.client_id;
        let picked = parse_osascript_pick("Session 2\n", &[c1, c2]);
        assert_eq!(picked, Some(expected));
    }

    #[test]
    fn parse_osascript_pick_returns_none_on_false() {
        let c1 = candidate("Session 1", 500);
        let picked = parse_osascript_pick("false\n", &[c1]);
        assert!(picked.is_none(), "cancellation must yield None");
    }

    #[test]
    fn parse_osascript_pick_returns_none_on_empty_stdout() {
        let c1 = candidate("Session 1", 500);
        let picked = parse_osascript_pick("", &[c1]);
        assert!(picked.is_none());
    }

    #[test]
    fn parse_osascript_pick_returns_none_on_unmatched_label() {
        // Defensive: an osascript output we cannot map back drops with
        // an ERROR log; no panic.
        let c1 = candidate("Session 1", 500);
        let picked = parse_osascript_pick("Session 7\n", &[c1]);
        assert!(picked.is_none());
    }

    #[test]
    fn parse_osascript_pick_trims_trailing_whitespace() {
        let c1 = candidate("Session 1", 500);
        let expected = c1.client_id;
        // Both \n and \r\n endings must work.
        assert_eq!(
            parse_osascript_pick("Session 1\n", std::slice::from_ref(&c1)),
            Some(expected)
        );
        assert_eq!(
            parse_osascript_pick("Session 1\r\n", std::slice::from_ref(&c1)),
            Some(expected)
        );
        // Leading/trailing spaces also trimmed.
        assert_eq!(
            parse_osascript_pick("  Session 1  \n", &[c1]),
            Some(expected)
        );
    }

    // ---------- pick_fallback_mra ----------

    #[test]
    fn pick_fallback_mra_picks_smallest_last_activity() {
        let c1 = candidate("Session 1", 1_000);
        let c2 = candidate("Session 2", 50);
        let c3 = candidate("Session 3", 500);
        let expected = c2.client_id;
        let picked = pick_fallback_mra(&[c1, c2, c3]);
        assert_eq!(picked, Some(expected));
    }

    #[test]
    fn pick_fallback_mra_handles_singleton() {
        let c1 = candidate("Session 1", 999);
        let expected = c1.client_id;
        let picked = pick_fallback_mra(std::slice::from_ref(&c1));
        assert_eq!(picked, Some(expected));
    }

    #[test]
    fn pick_fallback_mra_returns_none_on_empty() {
        assert!(pick_fallback_mra(&[]).is_none());
    }

    #[test]
    fn pick_fallback_mra_with_tie_picks_one_deterministically() {
        // When two candidates share the smallest last_activity, `min_by_key`
        // returns the FIRST one — deterministic but unspecified semantically.
        // Verify the helper does not panic and returns a uuid that
        // belongs to one of the tied candidates.
        let c1 = candidate("Session 1", 100);
        let c2 = candidate("Session 2", 100);
        let picked = pick_fallback_mra(&[c1.clone(), c2.clone()]);
        let chosen = picked.expect("non-empty");
        assert!(chosen == c1.client_id || chosen == c2.client_id);
    }

    // ---------- pick_candidate ----------

    #[test]
    fn pick_candidate_empty_returns_none() {
        assert!(pick_candidate(&[]).is_none());
    }
}
