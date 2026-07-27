//! Session titles, derived rather than generated.
//!
//! The first line of the first user prompt, trimmed, bounded, and **redacted**.
//! No model call: a title has to exist the instant a turn completes, has to work
//! offline, has to work in Local mode, and has to work for an Organisation with
//! no AI entitlement — which is the default state. Every one of those rules out
//! generation.
//!
//! The redaction is not defensive tidiness. The title is the single most visible
//! string in the product — it renders on the shared Organisation board — and
//! prompts are exactly where people paste a key to ask why it is failing.

/// Longest a title may be, in characters. Long enough to be a sentence, short
/// enough that a board row does not wrap.
pub const MAX_TITLE_CHARS: usize = 120;

/// Derive a title from a raw user prompt.
///
/// Returns `None` when there is nothing usable — an empty prompt, or one that
/// was entirely secret and redacted away to nothing meaningful. A Session with
/// no title is better than a Session titled `[REDACTED]`.
///
/// Calls the redactor **unguarded** — a pathological prompt that panics a regex
/// layer unwinds through here. The capture path must not use this directly; it
/// scrubs through its own `catch_unwind` guard and derives via
/// [`from_redacted`], so a redactor panic fails closed (no title, Session
/// flagged) instead of killing the capture worker. This entry point exists for
/// callers that have nowhere to route a redaction failure.
pub fn from_prompt(prompt: &str) -> Option<String> {
    from_redacted(&atlas_redact::redact(prompt).text)
}

/// Derive a title from an **already-redacted** prompt.
///
/// The redaction is the caller's job — deliberately, so the capture path can
/// run it inside the same panic guard that protects every other stored string.
/// Passing raw content here would put an unredacted title on the shared board.
pub fn from_redacted(text: &str) -> Option<String> {
    // First *non-empty* line, not first line: prompts routinely open with a
    // blank line or a pasted block, and titling a Session "```" helps nobody.
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;

    let stripped = strip_markdown_prefix(line);
    if stripped.is_empty() {
        return None;
    }

    Some(truncate(stripped, MAX_TITLE_CHARS))
}

/// Drop leading markdown noise so a prompt that opens with a heading or bullet
/// titles as its text rather than its punctuation.
fn strip_markdown_prefix(line: &str) -> &str {
    line.trim_start_matches(['#', '>', '-', '*', ' ', '\t'])
        .trim()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // Reserve one character for the ellipsis so the result is exactly bounded.
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    // Prefer cutting at a word boundary when one is close, so the title reads as
    // a truncated sentence rather than a truncated word. `rfind` returns a byte
    // index, so the "is it close enough" comparison counts the characters before
    // it rather than comparing bytes against a character budget — multi-byte
    // text would otherwise mis-place the cut.
    if let Some(space) = out.rfind(' ') {
        if out[..space].chars().count() > max_chars * 2 / 3 {
            out.truncate(space);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_becomes_the_title() {
        assert_eq!(
            from_prompt("Add rate limiting to the upload endpoint\n\nUse a token bucket."),
            Some("Add rate limiting to the upload endpoint".to_string())
        );
    }

    #[test]
    fn leading_blank_lines_are_skipped() {
        assert_eq!(
            from_prompt("\n\n   \nFix the flaky auth test"),
            Some("Fix the flaky auth test".to_string())
        );
    }

    #[test]
    fn markdown_prefixes_are_stripped() {
        assert_eq!(
            from_prompt("## Investigate the watcher"),
            Some("Investigate the watcher".to_string())
        );
        assert_eq!(
            from_prompt("- fix the rename detection"),
            Some("fix the rename detection".to_string())
        );
    }

    #[test]
    fn a_secret_pasted_into_the_prompt_never_reaches_the_title() {
        let title = from_prompt("here's my key sk-ABCDEF0123456789ABCDEF, why is it failing")
            .expect("title");
        assert!(!title.contains("sk-ABCDEF0123456789ABCDEF"), "{title}");
        assert!(title.contains("[REDACTED]"), "{title}");
    }

    #[test]
    fn a_long_prompt_is_bounded_and_ends_in_an_ellipsis() {
        let prompt = "word ".repeat(200);
        let title = from_prompt(&prompt).expect("title");
        assert!(title.chars().count() <= MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_long_prompt_is_cut_at_a_word_boundary_when_one_is_near() {
        let title = from_prompt(&format!("{} finally", "alpha beta gamma ".repeat(20)))
            .expect("title");
        assert!(!title.trim_end_matches('…').ends_with(' '));
        assert!(title.ends_with('…'));
    }

    #[test]
    fn an_empty_prompt_yields_no_title() {
        assert_eq!(from_prompt(""), None);
        assert_eq!(from_prompt("   \n\n  "), None);
        assert_eq!(from_prompt("###"), None);
    }

    #[test]
    fn a_title_is_deterministic() {
        let prompt = "Add rate limiting to the upload endpoint";
        assert_eq!(from_prompt(prompt), from_prompt(prompt));
    }

    #[test]
    fn from_redacted_derives_without_touching_the_redactor() {
        // The capture path scrubs under its own panic guard and hands the
        // scrubbed text here — the two entry points must agree on derivation.
        assert_eq!(
            from_redacted("Fix the flaky auth test"),
            Some("Fix the flaky auth test".to_string())
        );
        assert_eq!(
            from_prompt("Fix the flaky auth test"),
            from_redacted("Fix the flaky auth test")
        );
        assert_eq!(from_redacted("   \n  "), None);
    }

    #[test]
    fn multibyte_titles_truncate_on_character_boundaries() {
        // `rfind` returns a byte index; the boundary check must count chars.
        let prompt = "héllo wörld ".repeat(30);
        let title = from_prompt(&prompt).expect("title");
        assert!(title.chars().count() <= MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }
}
