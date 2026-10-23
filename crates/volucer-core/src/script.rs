//! What counts as a script a backend is asked to execute.
//!
//! Three separate places decide this question, and they must never disagree:
//!
//! 1. The router ([`volucer_fcgi::cgi::route`]) — a path with a script
//!    extension is a file to hand to the backend, anything else is application
//!    input for the front controller.
//! 2. The static layer ([`volucer_http::static_files`]) — a file with a script
//!    extension must never be served from disk, because that sends its *source*
//!    to the client. `index.php` is a valid response body, so nothing downstream
//!    would notice.
//! 3. `skip_stat` — a route whose `path_suffix` already pins a script extension
//!    proves the backend can only be handed a script, so the router's own
//!    `stat` may be skipped.
//!
//! The first two were once a shared `PHP_EXTENSIONS` constant and a function
//! that read it; the third had its own copy. This module is that constant made a
//! *value*, so a site can widen it for a non-PHP worker and all three move
//! together. Widening only the router would turn every newly-routable extension
//! into a source disclosure.
//!
//! Lives in `volucer-core` because `volucer-fcgi` and `volucer-watch` both
//! depend on it, and putting it anywhere else would be a dependency cycle.

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The extensions PHP executes when nothing is configured.
///
/// The same three the server has always accepted, and the same three a
/// `path_suffix` route was already allowed to claim. Absent configuration means
/// this list, so an existing deployment is unchanged on the wire and on disk.
pub const DEFAULT_SCRIPT_EXTENSIONS: [&str; 3] = ["php", "php3", "phtml"];

/// The extensions a path must end with to be treated as a script.
///
/// Stored without a leading dot and compared case-insensitively, which is what
/// PHP itself does — `/INDEX.PHP` is a script on every platform that matters.
/// The stored spelling is also what makes the type cheap to use: a router asks
/// "does this path end with any of these" on the request path, and a
/// [`ScriptExtensions`] with one entry is one comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScriptExtensions {
    extensions: Vec<String>,
}

impl Default for ScriptExtensions {
    fn default() -> Self {
        ScriptExtensions {
            extensions: DEFAULT_SCRIPT_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_string())
                .collect(),
        }
    }
}

impl ScriptExtensions {
    /// The list PHP's own default is, which is also the SDK-visible default.
    pub fn php() -> Self {
        Self::default()
    }

    /// A list from configuration, normalised.
    ///
    /// Accepts both spellings a person writes — `"php"` and `".php"` — because
    /// the two appear next to each other in this project's configuration
    /// already: `path_suffix = ".php"` carries the dot, and an extension in a
    /// list of extensions usually does not. Normalising once here means every
    /// reader, including `check`, sees the same thing.
    ///
    /// Empty is refused rather than accepted as "no scripts", because that
    /// would silently turn every path into application input — the front
    /// controller would receive `/index.php` as `PATH_INFO` — which is a
    /// configuration mistake that must be loud. A deployment that wants no
    /// backend at all has `serve_only = "static"` and no backend to send to.
    pub fn from_config(extensions: &[String], where_: &str) -> Result<Self> {
        if extensions.is_empty() {
            return Err(crate::error::Error::Config(format!(
                "{where_} is empty: name at least one extension, or remove the key to accept \
                 the defaults ({})",
                DEFAULT_SCRIPT_EXTENSIONS.join(", ")
            )));
        }

        let mut normalised = Vec::with_capacity(extensions.len());
        for extension in extensions {
            let trimmed = extension.trim();
            let bare = trimmed.strip_prefix('.').unwrap_or(trimmed);

            // A dot *inside* the extension is legal (`php3` has none, but a
            // deployment could serve `tar.gz`-style double extensions from a
            // worker), so the check is on the ends rather than on the whole.
            // What is refused is an extension that could never be the end of a
            // path: empty, or carrying a separator.
            if bare.is_empty() {
                return Err(crate::error::Error::Config(format!(
                    "{where_} contains an empty extension"
                )));
            }
            if bare.contains(['/', '\\']) {
                return Err(crate::error::Error::Config(format!(
                    "{where_} contains {trimmed:?}, which names a path rather than an extension"
                )));
            }

            let lower = bare.to_ascii_lowercase();
            if !normalised.contains(&lower) {
                normalised.push(lower);
            }
        }

        Ok(ScriptExtensions {
            extensions: normalised,
        })
    }

    /// The extensions, without the leading dot, lowercase.
    ///
    /// The one accessor a diagnostic needs: `check` prints the list so an
    /// operator can see that a site widened it, and the router's error messages
    /// name it.
    pub fn as_slice(&self) -> &[String] {
        &self.extensions
    }

    /// Whether this list is the default one.
    ///
    /// Used by `check` to say nothing when nothing was configured, rather than
    /// printing a line for a setting nobody wrote.
    pub fn is_default(&self) -> bool {
        self.extensions.len() == DEFAULT_SCRIPT_EXTENSIONS.len()
            && DEFAULT_SCRIPT_EXTENSIONS
                .iter()
                .all(|default| self.extensions.iter().any(|known| known == default))
    }

    /// The suffixes the static layer and the router compare against, with the
    /// leading dot restored.
    ///
    /// Returned rather than stored so there is one spelling in the type and one
    /// comparison in the hot path: a caller that needs `".php"` asks for it, and
    /// a caller that only needs to know "is this a script" calls
    /// [`ScriptExtensions::matches`].
    pub fn suffixes(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|extension| format!(".{extension}"))
            .collect()
    }

    /// Whether a path ends with one of these extensions, case-insensitively.
    ///
    /// Byte-wise rather than through `to_ascii_lowercase()` of the whole path:
    /// the previous implementation allocated a copy of the path per request to
    /// answer a question about its last few bytes, which is the defect that made
    /// this a predicate on bytes in the first place.
    ///
    /// Compared on bytes rather than on a `&str` suffix because a byte offset
    /// taken from the end of a `&str` can land inside a multi-byte character, and
    /// slicing there panics. A path with non-ASCII bytes in it must still be
    /// answerable.
    pub fn matches(&self, path: &str) -> bool {
        self.matches_bytes(path.as_bytes())
    }

    /// [`ScriptExtensions::matches`], on the bytes of a path.
    ///
    /// Exposed for `split_at_script`, which walks the boundaries of a path it
    /// already has as bytes and must not allocate a `&str` per candidate.
    pub fn matches_bytes(&self, bytes: &[u8]) -> bool {
        self.extensions.iter().any(|extension| {
            let length = extension.len() + 1;
            bytes.len() >= length
                && bytes[bytes.len() - length] == b'.'
                && bytes[bytes.len() - length + 1..].eq_ignore_ascii_case(extension.as_bytes())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn the_default_is_php() {
        let defaults = ScriptExtensions::default();
        assert!(defaults.is_default());
        assert!(defaults.matches("/index.php"));
        assert!(defaults.matches("/INDEX.PHP"));
        assert!(defaults.matches("/Index.Php3"));
        assert!(defaults.matches("index.phtml"));
        assert!(!defaults.matches("/index.php.bak"));
        assert!(!defaults.matches("/index.ph"));
        assert!(!defaults.matches(""));
        // A path may carry non-ASCII bytes, and the suffix comparison must not
        // slice at a byte offset that is not a character boundary.
        assert!(defaults.matches("/caf\u{e9}/index.php"));
        assert!(!defaults.matches("/caf\u{e9}"));
    }

    #[test]
    fn a_widened_list_routes_other_languages() {
        let widened =
            ScriptExtensions::from_config(&list(&["php", "py", "rb"]), "test").expect("valid");
        assert!(widened.matches("/app.py"));
        assert!(widened.matches("/app.rb"));
        assert!(widened.matches("/index.php"));
        assert!(!widened.matches("/app.js"));
        assert!(!widened.is_default());
    }

    #[test]
    fn the_leading_dot_is_optional_and_duplicates_collapse() {
        let parsed =
            ScriptExtensions::from_config(&list(&[".php", "PHP", "py", ".PY", " rb "]), "test")
                .expect("valid");
        assert_eq!(parsed.as_slice(), &["php", "py", "rb"]);
        assert_eq!(parsed.suffixes(), vec![".php", ".py", ".rb"]);
    }

    #[test]
    fn an_empty_list_is_refused_rather_than_meaning_no_scripts() {
        let err = ScriptExtensions::from_config(&[], "server.script_extensions")
            .expect_err("an empty list is a configuration mistake");
        assert!(
            err.to_string().contains("server.script_extensions"),
            "{err}"
        );
    }

    #[test]
    fn an_extension_that_names_a_path_is_refused() {
        for bad in [".", "  ", "py/sh", r"rb\sh"] {
            let err = ScriptExtensions::from_config(&list(&[bad]), "test")
                .expect_err("this is not an extension");
            assert!(err.to_string().contains("test"), "{err}");
        }
    }

    #[test]
    fn a_path_that_is_only_an_extension_still_matches_as_it_always_did() {
        // `.php` is a legal (if odd) filename, and the predicate that existed
        // before this type did accepted it. Keeping that is not an oversight:
        // making the *list* configurable is the point, and a predicate that
        // changed answer along the way would be a routing change nobody asked
        // for. The front controller's dotfile rule is what keeps such a path
        // from becoming application input.
        assert!(ScriptExtensions::default().matches(".php"));
        assert!(ScriptExtensions::default().matches("/.php"));
    }
}
