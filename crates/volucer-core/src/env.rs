//! `${VARIABLE}` expansion, applied to the configuration file before it is parsed.
//!
//! # Why before parsing, and not on the parsed values
//!
//! Expanding a *value* after deserialization cannot reach a non-string position. TOML has no
//! unquoted placeholder, so `workers = ${WORKERS}` is a parse error at the tokenizer, and the
//! quoted form `workers = "${WORKERS}"` is a type error at serde (`invalid type: string "4",
//! expected u32` — measured, not assumed). Substituting into the **text** is the only form that
//! makes a number, a port or a listener address expressible, and those are exactly the keys a
//! container wants to vary.
//!
//! # Braced form only
//!
//! `${VARIABLE}` and `${VARIABLE:-default}`. A bare `$VARIABLE` is **not** expanded, which is a
//! deliberate divergence from RoadRunner: an access-log value, a password or a DSN may legitimately
//! contain `$`, and a rule that expands it silently makes a configuration mean something its author
//! did not write. `$$` is the escape — it produces one `$` — so a literal `${NAME}` is written
//! `$${NAME}`, the same spelling Docker Compose uses for the same problem.
//!
//! # A `${` in a comment is expanded too
//!
//! The substitution is textual, so it does not know a comment from a value. Documenting
//! `$${VAR}` in a comment is therefore written `$$${VAR}`-free — see the tests — and an operator
//! who writes `${VAR}` in a comment gets the same value substituted there. That is the cost of
//! one rule instead of two, and it is why a **missing** variable is a startup error: a silent
//! empty string in a listener address would be a server that binds nothing for a reason nothing logs.

use std::collections::BTreeMap;

/// The character a reference starts with.
const SIGIL: char = '$';

/// Expand every `${...}` reference in `text`, reading values from the process environment.
pub fn expand(text: &str) -> Result<String, String> {
    expand_with(text, |name| std::env::var(name).ok())
}

/// [`expand`], with the environment supplied by the caller.
///
/// The lookup is a parameter so that a test does not have to write the process environment —
/// which is global, and a test that writes it races every other test in the binary.
pub fn expand_with<F>(text: &str, lookup: F) -> Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some((offset, character)) = chars.next() {
        if character != SIGIL {
            out.push(character);
            continue;
        }

        match chars.peek() {
            // `$$` is one literal `$`, so `$${NAME}` is the text `${NAME}`.
            Some((_, SIGIL)) => {
                chars.next();
                out.push(SIGIL);
            }
            Some((_, '{')) => {
                chars.next();
                let mut body = String::new();
                let mut closed = false;
                for (_, character) in chars.by_ref() {
                    if character == '}' {
                        closed = true;
                        break;
                    }
                    body.push(character);
                }
                if !closed {
                    return Err(format!(
                        "unterminated ${{...}} starting on line {}; write $${{ for a literal ${{",
                        line_of(text, offset)
                    ));
                }
                out.push_str(&resolve(&body, text, offset, &lookup)?);
            }
            _ => out.push(SIGIL),
        }
    }

    Ok(out)
}

/// Turn one reference's body — `NAME` or `NAME:-default` — into the text that replaces it.
fn resolve<F>(body: &str, text: &str, offset: usize, lookup: &F) -> Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let (name, default) = match body.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (body, None),
    };

    if !is_valid_name(name) {
        return Err(format!(
            "${{{body}}} on line {} names no environment variable; a name is letters, digits and \
             underscores and may not start with a digit",
            line_of(text, offset)
        ));
    }
    if default.is_some_and(|default| default.contains("${")) {
        // A nested reference would need the closing brace to belong to the inner one, and this
        // scanner stops at the first `}` — so accepting it would silently produce a truncated
        // default rather than a nesting.
        return Err(format!(
            "${{{body}}} on line {} nests a reference inside its default, which is not supported; \
             expand the inner value in the environment instead",
            line_of(text, offset)
        ));
    }

    let value = match lookup(name) {
        // `:-` is the *unset or empty* form, which is bash's meaning and RoadRunner's.
        Some(value) if !(default.is_some() && value.is_empty()) => value,
        _ => match default {
            Some(default) => default.to_string(),
            None => {
                return Err(format!(
                    "environment variable {name} is not set (used on line {}); export it, or write \
                     ${{{name}:-a-default}} to make it optional",
                    line_of(text, offset)
                ))
            }
        },
    };

    // The substitution happens before the file is parsed, so a value carrying a newline could
    // introduce a table header, a key or a comment into a document that does not contain one. A
    // NUL would make the process environment's own encoding ambiguous.
    if value.contains(['\n', '\r', '\0']) {
        return Err(format!(
            "the expansion of {name} on line {} contains a line break or a NUL byte; a \
             configuration value is substituted into the file's text, so it may not add lines",
            line_of(text, offset)
        ));
    }

    Ok(value)
}

/// Whether `name` is a POSIX-ish variable name: `[A-Za-z_][A-Za-z0-9_]*`.
///
/// Stricter than what a shell accepts is the point: a name containing a `-` or a space is a typo,
/// and reporting it as "not set" would send the reader looking in their environment rather than at
/// the line they wrote.
fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// The 1-based line an offset falls on, for messages an operator can act on.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

/// Refuse an `env` table whose keys or values a process cannot be started with.
///
/// `Command::env` accepts anything until the process is spawned, at which point a key containing
/// `=` fails with an `InvalidInput` error that names no key — a startup failure an operator cannot
/// trace back to their file. Refused here instead, naming the entry.
///
/// Shared by every table that takes an `env` map, so a new one cannot be added without the rule.
pub fn check_env_table(
    what: &str,
    env: &BTreeMap<String, String>,
) -> std::result::Result<(), String> {
    for (key, value) in env {
        if key.is_empty() {
            return Err(format!("{what} has an empty variable name"));
        }
        if key.contains(['=', '\0']) {
            return Err(format!(
                "{what} sets {key:?}, which is not a usable variable name: a name may not contain \
                 '=' or a NUL byte"
            ));
        }
        if value.contains('\0') {
            return Err(format!(
                "{what} sets {key} to a value containing a NUL byte, which cannot be passed to a \
                 process"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_fixture(text: &str) -> std::result::Result<String, String> {
        expand_with(text, |name| match name {
            "HTTP_PORT" => Some("8081".to_string()),
            "APP_ENV" => Some("staging".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        })
    }

    #[test]
    fn a_reference_is_replaced_by_the_value() {
        assert_eq!(
            "address = \"127.0.0.1:8081\"",
            expand_fixture("address = \"127.0.0.1:${HTTP_PORT}\"").unwrap()
        );
    }

    /// The whole reason the substitution is textual: a number position must work.
    #[test]
    fn a_reference_in_a_number_position_becomes_a_number() {
        let text = "workers = ${HTTP_PORT}";
        assert_eq!("workers = 8081", expand_fixture(text).unwrap());
        // And the result is a number to TOML rather than a string — the property the post-parse
        // approach could not have.
        let value: toml::Value = toml::from_str(&expand_fixture(text).unwrap()).unwrap();
        assert!(value.get("workers").unwrap().is_integer());
    }

    #[test]
    fn a_default_applies_to_an_unset_variable() {
        assert_eq!(
            "log = \"info\"",
            expand_fixture("log = \"${MISSING:-info}\"").unwrap()
        );
    }

    /// bash's `:-` means *unset or empty*, and an empty value is the case a template produces.
    #[test]
    fn a_default_applies_to_an_empty_variable_too() {
        assert_eq!(
            "log = \"info\"",
            expand_fixture("log = \"${EMPTY:-info}\"").unwrap()
        );
        assert_eq!(
            "log = \"staging\"",
            expand_fixture("log = \"${APP_ENV:-info}\"").unwrap()
        );
    }

    #[test]
    fn an_empty_default_is_an_empty_value() {
        assert_eq!(
            "log = \"\"",
            expand_fixture("log = \"${MISSING:-}\"").unwrap()
        );
    }

    /// An unset variable without a default is an error, not an empty string: an empty listener
    /// address would be a server that binds nothing for a reason nothing logs.
    #[test]
    fn an_unset_variable_with_no_default_is_refused() {
        let err = expand_fixture("http = \"0.0.0.0:${MISSING}\"").unwrap_err();
        assert!(err.contains("MISSING"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains(":-"), "and it names the fix: {err}");
    }

    #[test]
    fn a_reference_on_a_later_line_reports_that_line() {
        let err = expand_fixture("a = 1\n\nb = \"${MISSING}\"").unwrap_err();
        assert!(err.contains("line 3"), "{err}");
    }

    #[test]
    fn an_unterminated_reference_is_refused() {
        let err = expand_fixture("http = \"${HTTP_PORT\"").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn a_name_that_is_not_a_variable_name_is_refused() {
        for body in ["", "1PORT", "APP-PORT", "APP PORT", "APP.PORT"] {
            let text = format!("x = \"${{{body}}}\"");
            let err = expand_fixture(&text).unwrap_err();
            assert!(
                err.contains("names no environment variable"),
                "{body}: {err}"
            );
        }
        // And the names a shell would accept are accepted.
        for body in ["_X", "A1", "APP_ENV"] {
            assert!(
                expand_fixture(&format!("x = \"${{{body}:-d}}\"")).is_ok(),
                "{body}"
            );
        }
    }

    #[test]
    fn a_nested_default_is_refused_rather_than_truncated() {
        let err = expand_fixture("x = \"${MISSING:-${APP_ENV}}\"").unwrap_err();
        assert!(err.contains("nests"), "{err}");
    }

    /// `$$` is the escape, so a literal `${NAME}` is writable.
    #[test]
    fn a_doubled_dollar_is_one_literal_dollar() {
        assert_eq!("$", expand_fixture("$").unwrap());
        assert_eq!("$$", expand_fixture("$$$").unwrap());
        assert_eq!("${HTTP_PORT}", expand_fixture("$${HTTP_PORT}").unwrap());
        // And a `$` that starts nothing is left alone rather than refused.
        assert_eq!("pa$$word", expand_fixture("pa$$$word").unwrap());
        assert_eq!("a$b", expand_fixture("a$b").unwrap());
    }

    #[test]
    fn a_value_with_a_line_break_is_refused() {
        let err = expand_with("x = \"${BAD}\"", |_| Some("a\n[server]".to_string())).unwrap_err();
        assert!(err.contains("line break"), "{err}");
    }

    #[test]
    fn an_env_table_refuses_what_a_process_cannot_be_started_with() {
        let ok = BTreeMap::from([("APP_ENV".to_string(), "prod".to_string())]);
        check_env_table("server.env", &ok).expect("a plain pair");

        for (key, value) in [("A=B", "x"), ("", "x"), ("A", "a\0b")] {
            let bad = BTreeMap::from([(key.to_string(), value.to_string())]);
            let err = check_env_table("server.env", &bad).unwrap_err();
            assert!(err.starts_with("server.env"), "{key:?}: {err}");
        }
    }
}
