//! Reading the gRPC services a `.proto` file declares.
//!
//! **This is not a protobuf parser, and it does not need to be one.** A service's fully-qualified
//! name is decided by two things in its own file — the file-scope `package` and a top-level
//! `service` — and by nothing else. `import` exists to resolve *message and field types*, which this
//! never looks at, so an import is a line to skip rather than a file to follow; and protobuf does
//! not allow a nested `service`, so "`service` at brace depth zero" is the whole of the grammar a
//! grant needs. A real parser (a descriptor set, or `protox`) would buy the ability to read types,
//! which is exactly the information an allow-list of *names* must not depend on.
//!
//! What it does have to get right is the three ways a text can lie about its own structure:
//!
//! - a `service` mentioned in a comment is prose, not a declaration;
//! - a `}` inside a string literal does not close a block;
//! - a `message`/`enum`/`oneof`/`extend`/`rpc` body can contain the word `service` as a field name,
//!   and depth is what separates those from a declaration.
//!
//! All three are handled in [`strip_comments_and_literals`], which is the only state machine here,
//! so there is one place for the next surprise rather than three.

use crate::config::is_valid_grpc_service;

/// The protobuf service names a `.proto` file declares, fully qualified and in file order.
///
/// Qualified by the file's own `package`, because that is what a `:path` carries: a declaration of
/// `service Greeter` under `package greeter` is the service `greeter.Greeter`, and a grant of
/// `Greeter` would be a name no request could ever address.
///
/// A name that is not a legal protobuf full name is **dropped rather than returned**, so this
/// function cannot produce a grant the router would refuse. A file whose every service is dropped
/// yields nothing, which the caller reports as "resolved no service" rather than as silence.
pub fn service_names(text: &str) -> Vec<String> {
    let cleaned = strip_comments_and_literals(text);
    let bytes = cleaned.as_bytes();

    let mut services: Vec<String> = Vec::new();
    // Three states rather than `Option<String>`, because "no package" and "a package that is not a
    // name" have to answer differently. A file with no `package` declares a bare service name,
    // which is legal protobuf. A file with `package 9bad;` is one `protoc` would refuse, and
    // falling back to the bare name there would grant `Greeter` for a file that declares something
    // else entirely — so its services are dropped, and the caller reports a source that resolved
    // nothing rather than one that resolved the wrong thing.
    enum Package {
        Absent,
        Named(String),
        Malformed,
    }
    let mut package = Package::Absent;
    let mut depth = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            byte if is_identifier_start(byte) => {
                let (word, next) = read_identifier(&cleaned, i);
                i = next;
                // Everything below is a *declaration*, and every declaration this cares about is
                // file-scope. Depth zero is what makes a field named `service` inside a message
                // something other than a declaration.
                if depth != 0 {
                    continue;
                }
                match word {
                    // One `package` per file, and the first is the one protoc uses.
                    "package" if matches!(package, Package::Absent) => {
                        let (name, next) = read_dotted_name(&cleaned, i);
                        i = next;
                        package = match name {
                            Some(name) => Package::Named(name),
                            None => Package::Malformed,
                        };
                    }
                    "service" => {
                        let start = skip_whitespace(bytes, i);
                        if start >= bytes.len() || !is_identifier_start(bytes[start]) {
                            continue;
                        }
                        let (name, next) = read_identifier(&cleaned, start);
                        i = next;
                        let qualified = match &package {
                            Package::Absent => name.to_string(),
                            Package::Named(package) => format!("{package}.{name}"),
                            Package::Malformed => continue,
                        };
                        if is_valid_grpc_service(&qualified) && !services.contains(&qualified) {
                            services.push(qualified);
                        }
                    }
                    _ => {}
                }
            }
            _ => i += 1,
        }
    }

    services
}

/// The text with every comment removed and every string literal replaced by a space.
///
/// Both are removed for the same reason: they are the only places a brace, a semicolon or the word
/// `service` can appear without meaning what it says. Newlines inside a block comment are kept so
/// that a byte offset in the output still points at the same line as the input, which is what makes
/// a diagnostic about this text readable.
///
/// Bytes above ASCII are copied through `as char`, which turns a multi-byte sequence into the
/// corresponding Latin-1 characters. That is deliberate and harmless here: no non-ASCII character is
/// an identifier byte, so the difference is invisible to the scan, and decoding properly would mean
/// this function had to fail on a file whose *comments* are not UTF-8.
fn strip_comments_and_literals(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() {
                    // An escaped quote does not end the literal, and a backslash at the very end of
                    // the file must not read past it.
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    let ended = bytes[i] == quote;
                    i += 1;
                    if ended {
                        break;
                    }
                }
                out.push(' ');
            }
            byte => {
                out.push(byte as char);
                i += 1;
            }
        }
    }

    out
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// The identifier that starts at `start`, and the offset just past it.
fn read_identifier(text: &str, start: usize) -> (&str, usize) {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    (&text[start..end], end)
}

/// A dotted name (`foo.bar.Baz`), for a `package` statement.
///
/// Whitespace is allowed around the dots, which is legal protobuf and the reason this is not
/// `read_identifier` plus a `starts_with`. `None` when there is no name at all, so `package;` leaves
/// the file unqualified rather than qualifying its services with an empty prefix.
fn read_dotted_name(text: &str, start: usize) -> (Option<String>, usize) {
    let bytes = text.as_bytes();
    let mut name = String::new();
    let mut i = skip_whitespace(bytes, start);

    loop {
        if i >= bytes.len() || !is_identifier_start(bytes[i]) {
            break;
        }
        let (part, next) = read_identifier(text, i);
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(part);
        i = skip_whitespace(bytes, next);
        if i < bytes.len() && bytes[i] == b'.' {
            i = skip_whitespace(bytes, i + 1);
            continue;
        }
        break;
    }

    (if name.is_empty() { None } else { Some(name) }, i)
}

fn skip_whitespace(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packaged_service_is_qualified_by_its_package() {
        let text = r#"
syntax = "proto3";
package greeter;

service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
}
"#;
        assert_eq!(service_names(text), ["greeter.Greeter"]);
    }

    #[test]
    fn a_file_without_a_package_declares_a_bare_name() {
        let text = "service Health { rpc Check (Ping) returns (Pong); }";
        assert_eq!(service_names(text), ["Health"]);
    }

    #[test]
    fn a_dotted_package_is_joined_with_dots() {
        let text = "package acme.api . v1 ;\nservice Thing {}";
        assert_eq!(service_names(text), ["acme.api.v1.Thing"]);
    }

    #[test]
    fn every_service_in_a_file_is_found_and_order_is_the_files_own() {
        let text = r#"
package multi;
service A {}
service B {}
"#;
        assert_eq!(service_names(text), ["multi.A", "multi.B"]);
    }

    #[test]
    fn a_service_named_in_a_comment_is_prose_not_a_declaration() {
        let text = r#"
package greeter;
// service NotAService {}
/* service AlsoNot {
     service Nested {}
   } */
service Greeter {}
"#;
        assert_eq!(service_names(text), ["greeter.Greeter"]);
    }

    #[test]
    fn a_brace_inside_a_string_does_not_close_a_block() {
        let text = r#"
package greeter;
option go_package = "github.com/acme/gen};{";
option something = 'service Nope {}';
service Greeter {}
"#;
        assert_eq!(service_names(text), ["greeter.Greeter"]);
    }

    #[test]
    fn a_field_named_service_inside_a_message_is_not_a_declaration() {
        let text = r#"
package greeter;
message Config {
  string service = 1;
  bool package = 2;
}
service Greeter {}
"#;
        assert_eq!(service_names(text), ["greeter.Greeter"]);
    }

    #[test]
    fn a_real_field_called_service_outside_a_message_is_still_not_a_declaration() {
        // Depth zero with no `service` identifier following it: the scan must not read `= 1` as a
        // name. This is a malformed file, and the answer is "no service", not a grant of `1`.
        let text = "package greeter;\nservice = 1;\n";
        assert!(service_names(text).is_empty());
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string_early() {
        let text = r#"
package greeter;
option x = "a\"service Nope {";
service Greeter {}
"#;
        assert_eq!(service_names(text), ["greeter.Greeter"]);
    }

    #[test]
    fn a_name_that_is_not_a_legal_service_is_dropped() {
        // A package cannot start with a digit in protobuf, so this file would not compile; the scan
        // must not turn it into a grant the router could never match.
        let text = "package 9bad;\nservice Greeter {}";
        assert!(service_names(text).is_empty());
    }

    #[test]
    fn a_file_with_no_service_resolves_nothing() {
        assert!(service_names("syntax = \"proto3\";\nmessage A {}").is_empty());
        assert!(service_names("").is_empty());
    }

    #[test]
    fn stripping_removes_comments_and_literals_but_keeps_the_line_count() {
        let text = "a // comment\nb /* x\ny */ \"str\" c";
        let cleaned = strip_comments_and_literals(text);
        assert_eq!(cleaned.matches('\n').count(), 2);
        assert!(cleaned.contains('a') && cleaned.contains('b') && cleaned.contains('c'));
        assert!(!cleaned.contains("comment"));
        assert!(!cleaned.contains("str"));
    }
}
