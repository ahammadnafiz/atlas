//! Layer 6 — bounded credential key/value pairs.
//!
//! `API_KEY=supersecretvalue`, `GITHUB_TOKEN: ghp_…`, `db_password="hunter2"`.
//! The value carries no recognisable format and may be a memorable low-entropy
//! word, so nothing above catches it — but the *key* names it as a credential,
//! and that is evidence enough.
//!
//! Only the value is replaced. `API_KEY=[REDACTED]` still tells a reader which
//! variable was involved, which is most of what makes a transcript useful for
//! debugging; blanking the whole assignment throws that away for no extra
//! safety.
//!
//! This is the layer most exposed to false positives, so it is bounded three
//! ways: the key must be a whole identifier token (not a substring of a longer
//! word), a bare `:` separator is only honoured for identifier-shaped keys or a
//! quoted value so ordinary prose like "the secret: it was a race" is
//! untouched, and the value must survive the placeholder gate.
//!
//! Quoted keys — `"password": "hunter2"`, the JSON spelling — are matched too.
//! JSON *documents* go through the structure-preserving walker, but a JSON
//! fragment quoted inside a prose body never parses as a document, and this
//! layer is the only thing standing between such a fragment and disk.

use std::sync::OnceLock;

use regex::Regex;

use crate::placeholder::is_real_secret_value;
use crate::region::Region;
use crate::Category;

/// Shortest value worth flagging. Below this the "secret" is a flag or an enum
/// (`token=1`, `auth=on`) far more often than a credential.
const MIN_VALUE_LEN: usize = 4;

/// Key segments that name a credential outright.
const SECRET_WORDS: &[&str] = &[
    "secret",
    "secrets",
    "token",
    "tokens",
    "password",
    "passwords",
    "passwd",
    "passphrase",
    "credential",
    "credentials",
    "apikey",
    "authtoken",
];

/// `key` is far too common to stand alone — `cache_key`, `primary_key`,
/// `sort_key` are all ordinary. It only names a credential when qualified.
const KEY_QUALIFIERS: &[&str] = &[
    "api", "secret", "access", "private", "signing", "encryption", "auth", "client", "app",
    "service", "license", "subscription", "consumer", "publishable",
];

/// Any assignment at all. Which of them is a credential is decided in code
/// below, because the deciding rule — *the key must end in a credential word* —
/// is not something a single regex expresses without becoming unreadable and
/// unmaintainable.
fn assignment() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:^|[^A-Za-z0-9_])([A-Za-z0-9_.-]{1,64})[ \t]*(=|:)[ \t]*("[^"]*"|'[^']*'|[^\s,;&]+)"#,
        )
        .expect("static pattern")
    })
}

/// The JSON spelling of an assignment: `"password": "hunter2"`. The pattern
/// above cannot see it — the closing quote sits between the key and the colon —
/// and a JSON fragment pasted into prose never reaches the JSON-aware walker,
/// so without this form a `{"password": "hunter2"}` snippet inside a chat
/// message would pass through the flat redactor untouched.
///
/// A quoted key needs no identifier-shape gate: the quotes themselves are the
/// evidence that this is config or data, not a word in a sentence.
fn quoted_key_assignment() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:"([A-Za-z0-9_.-]{1,64})"|'([A-Za-z0-9_.-]{1,64})')[ \t]*:[ \t]*("[^"]*"|'[^']*'|[^\s,;&}\]]+)"#,
        )
        .expect("static pattern")
    })
}

/// Does this key name a credential?
///
/// The rule is that a credential word must be the key's **final** segment, which
/// is what separates `GITHUB_TOKEN` and `db_password` from `token_count`,
/// `secret_santa_name` and `password_policy` — all of which a substring match
/// would happily redact. Segments split on `_`, `-`, `.` and camelCase humps, so
/// `authToken` and `x-api-key` classify the same as their snake-case spellings.
fn is_credential_key(key: &str) -> bool {
    let segments = segments(key);
    let Some(last) = segments.last() else {
        return false;
    };

    if SECRET_WORDS.contains(&last.as_str()) {
        return true;
    }
    // Bare `PWD` is the shell's present-working-directory variable; redacting
    // every developer's `PWD=/Users/…` would be a daily, visible wrong answer.
    // Qualified (`db_pwd`, `mysql-pwd`) it is a password.
    if last == "pwd" && segments.len() > 1 {
        return true;
    }
    if matches!(last.as_str(), "key" | "keys") {
        return segments
            .get(segments.len().wrapping_sub(2))
            .is_some_and(|prev| KEY_QUALIFIERS.contains(&prev.as_str()));
    }
    false
}

/// Lowercase segments of a key, split on separators and camelCase humps.
fn segments(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut prev_lower = false;
    for ch in key.chars() {
        if matches!(ch, '_' | '-' | '.') {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_ascii_uppercase() && prev_lower && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        current.push(ch.to_ascii_lowercase());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

pub(crate) fn detect(input: &str) -> Vec<Region> {
    let mut regions = Vec::new();
    for captures in assignment().captures_iter(input) {
        let (Some(key), Some(separator), Some(value)) =
            (captures.get(1), captures.get(2), captures.get(3))
        else {
            continue;
        };

        if !is_credential_key(key.as_str()) {
            continue;
        }
        // A bare `:` after a lone credential word is usually prose — unless the
        // value is quoted, which is the YAML spelling (`password: "hunter2"`);
        // prose does not quote what follows its colon.
        if separator.as_str() == ":"
            && !is_identifier_shaped(key.as_str())
            && !is_quoted(value.as_str())
        {
            continue;
        }

        regions.extend(value_region(input, value));
    }
    for captures in quoted_key_assignment().captures_iter(input) {
        let (Some(key), Some(value)) =
            (captures.get(1).or_else(|| captures.get(2)), captures.get(3))
        else {
            continue;
        };
        if !is_credential_key(key.as_str()) {
            continue;
        }
        regions.extend(value_region(input, value));
    }
    regions
}

/// Vet a matched value and turn it into a region, or decline: too short, purely
/// numeric, or a placeholder.
fn value_region(input: &str, value: regex::Match) -> Option<Region> {
    let (start, end) = strip_quotes(input, value.start(), value.end());
    let raw = &input[start..end];
    if raw.len() < MIN_VALUE_LEN || is_numeric(raw) || !is_real_secret_value(raw) {
        return None;
    }
    Some(Region::new(start, end, Category::CredentialValue))
}

/// A purely numeric value is a port, a count, a limit — `max_tokens=4096`,
/// `"expires_in": 3600` — never a credential, whatever its key is called.
fn is_numeric(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Is the raw matched value wrapped in quotes?
fn is_quoted(value: &str) -> bool {
    value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
}

/// A key that reads as a variable rather than a word in a sentence: it is
/// multi-segment (`api_key`, `authToken`, `db-password`) or shouted (`SECRET`).
///
/// This gate applies only to the `:` separator, where a credential word is just
/// as likely to be prose — "the secret: it was a race condition" — as a config
/// key. `=` needs no such gate; nobody writes an equals sign in a sentence.
fn is_identifier_shaped(key: &str) -> bool {
    segments(key).len() > 1
        || (key.chars().any(|c| c.is_ascii_alphabetic())
            && !key.chars().any(|c| c.is_ascii_lowercase()))
}

/// Narrow a matched value to the inside of its quotes, so the quotes survive
/// replacement and the surrounding syntax stays valid.
fn strip_quotes(input: &str, start: usize, end: usize) -> (usize, usize) {
    if end - start < 2 {
        return (start, end);
    }
    let bytes = input.as_bytes();
    let (first, last) = (bytes[start], bytes[end - 1]);
    if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
        return (start + 1, end - 1);
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(input: &str) -> Vec<&str> {
        detect(input)
            .into_iter()
            .map(|r| &input[r.start..r.end])
            .collect()
    }

    #[test]
    fn an_env_assignment_flags_only_the_value() {
        assert_eq!(spans("API_KEY=supersecretvalue123"), vec!["supersecretvalue123"]);
    }

    #[test]
    fn a_quoted_value_is_flagged_inside_its_quotes() {
        assert_eq!(spans(r#"db_password="hunter2xyz""#), vec!["hunter2xyz"]);
    }

    #[test]
    fn a_colon_separator_works_for_identifier_shaped_keys() {
        assert_eq!(spans("GITHUB_TOKEN: abcdefgh1234"), vec!["abcdefgh1234"]);
        assert_eq!(spans("api_key: abcdefgh1234"), vec!["abcdefgh1234"]);
    }

    #[test]
    fn prose_using_a_credential_word_before_a_colon_is_left_alone() {
        assert!(spans("the secret: it was a race condition all along").is_empty());
        assert!(spans("token: the parser emits one per line").is_empty());
    }

    #[test]
    fn the_present_working_directory_variable_is_not_a_password() {
        assert!(spans("PWD=/Users/nafiz/Development/atlas").is_empty());
    }

    #[test]
    fn a_prefixed_pwd_key_is_still_a_password() {
        assert_eq!(spans("db_pwd=hunter2xyz"), vec!["hunter2xyz"]);
    }

    #[test]
    fn a_key_that_merely_contains_the_word_is_not_matched_mid_identifier() {
        // `mypasswordhelper` is one identifier; the boundary rule requires the
        // key to be a whole token, and here the whole token is still matched —
        // what must not happen is matching only the `password` substring.
        let found = spans("notatoken_field=value1234");
        assert!(found.is_empty(), "unexpected: {found:?}");
    }

    #[test]
    fn placeholder_values_are_left_alone() {
        assert!(spans("API_KEY=<your-api-key>").is_empty());
        assert!(spans("db_password=changeme").is_empty());
        assert!(spans("API_KEY=[REDACTED]").is_empty());
    }

    #[test]
    fn very_short_values_are_left_alone() {
        assert!(spans("token=1").is_empty());
    }

    #[test]
    fn a_json_quoted_key_is_caught_by_the_flat_layer() {
        for (input, expected) in [
            (r#"{"password": "hunter2"}"#, "hunter2"),
            (r#"{'password': 'hunter2'}"#, "hunter2"),
            (r#""client_secret":"hunter2xyz""#, "hunter2xyz"),
            (r#"the tool sent {"api_key": "abcdefgh1234"} and failed"#, "abcdefgh1234"),
        ] {
            assert_eq!(spans(input), vec![expected], "missed: {input}");
        }
    }

    #[test]
    fn a_yaml_credential_key_with_a_quoted_value_is_caught() {
        assert_eq!(spans(r#"password: "hunter2""#), vec!["hunter2"]);
        assert_eq!(spans("secret: 'correct-horse-battery'"), vec!["correct-horse-battery"]);
    }

    #[test]
    fn quoted_non_credential_keys_are_left_alone() {
        for input in [
            r#"{"message_id": "xJ3kQ9vB2mZ7pL5rT8wN4cF6yH1sD0gA"}"#,
            r#"{"role": "assistant", "content": "done"}"#,
            r#"{"password_policy": "strict-rotation"}"#,
            r#"{"token_count": "many"}"#,
        ] {
            assert!(spans(input).is_empty(), "over-redacted: {input}");
        }
    }

    #[test]
    fn quoted_credential_keys_with_placeholder_values_are_left_alone() {
        assert!(spans(r#"{"password": "[REDACTED]"}"#).is_empty());
        assert!(spans(r#"{"api_key": "<your-api-key>"}"#).is_empty());
        assert!(spans(r#"{"client_secret": "changeme"}"#).is_empty());
    }

    #[test]
    fn purely_numeric_values_are_not_credentials() {
        for input in [
            r#"{"max_tokens": 4096}"#,
            "max_tokens=4096",
            r#""tokens": 51234"#,
            "request_tokens: 8192.5",
        ] {
            assert!(spans(input).is_empty(), "over-redacted: {input}");
        }
    }
}
