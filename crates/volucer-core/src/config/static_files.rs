//! `[server.static_files]`: how files under a document root are served.

use super::*;

/// How files under the document root are served.
///
/// The HTTP front-end only. A file goes to a backend when nothing here matches,
/// which is what keeps a front controller working: with `index.php` configured,
/// a path that names no file is PHP's business, not this layer's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticConfig {
    /// Serve files at all. Off by default, so an existing deployment is not
    /// changed by upgrading: a request that used to reach the front controller
    /// still does.
    #[serde(default)]
    pub enabled: bool,

    /// Serve `file.gz` or `file.br` when the client accepts that encoding.
    ///
    /// Opt-in because it assumes the build produced those files, and silently
    /// serving a stale one is worse than not serving it at all.
    #[serde(default)]
    pub precompressed: bool,

    /// Serve `index.html` for a directory, with a redirect when the URL has no
    /// trailing slash. When off, a directory is not a file and so goes to PHP —
    /// which is what an application with no static build wants.
    #[serde(default = "default_true")]
    pub directory_index: bool,

    /// What to do with a dotfile such as `.git/config` or `.env`.
    #[serde(default)]
    pub dotfiles: Dotfiles,

    /// `Cache-Control` for files served from here.
    ///
    /// One value for everything, deliberately: a per-extension policy is a
    /// guess about someone else's build, and a wrong `immutable` is invisible
    /// until assets go stale.
    #[serde(default = "default_cache_control")]
    pub cache_control: String,

    /// Refuse a multi-range request larger than this many bytes.
    ///
    /// A multipart range answer is assembled in memory, and `bytes=0-,0-,0-,…`
    /// is a cheap way to ask a server to allocate. Single ranges and `Range:
    /// bytes=0-` are unaffected: they are answered by streaming.
    #[serde(default = "default_max_multirange")]
    pub max_multirange_bytes: u64,
}

impl Default for StaticConfig {
    fn default() -> Self {
        StaticConfig {
            enabled: false,
            precompressed: false,
            directory_index: true,
            dotfiles: Dotfiles::Deny,
            cache_control: default_cache_control(),
            max_multirange_bytes: default_max_multirange(),
        }
    }
}

/// What to do when a request path contains a dotfile component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dotfiles {
    /// 404, matching how nginx treats a hidden file by default. Not 403: which
    /// files exist is not something a client needs to learn.
    #[default]
    Deny,
    /// Treat them like any other file. Only sensible when the document root
    /// holds nothing but published assets.
    Allow,
}

fn default_cache_control() -> String {
    "public, max-age=3600".to_string()
}

fn default_max_multirange() -> u64 {
    1024 * 1024
}
