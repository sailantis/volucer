//! Error and result types shared across volucer crates.

/// Errors produced while loading configuration or inspecting the host.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to read {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse configuration: {0}")]
    Toml(#[from] toml::de::Error),

    /// One file of a multi-file configuration is not TOML.
    ///
    /// Its own variant rather than [`Error::Toml`] because a file reached through `include` or
    /// `include_dir` has a name the failed parse's own message does not carry — and "failed to
    /// parse configuration: expected `=` on line 3" sends an operator to their main file when the
    /// mistake is in a drop-in they may not have written.
    ///
    /// **The source is boxed**, which [`Error::Read`] does not need: `toml::de::Error` is 88
    /// bytes, so carrying it inline would make this variant three times the size of any other and
    /// grow every `Result` in the workspace — enough for `ServerError` to cross clippy's
    /// 128-byte `result_large_err` line. A parse error is read once and never matched on, so a
    /// pointer costs nothing an operator can see.
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PHP binary {path} produced no version output")]
    PhpUnrecognizedVersion { path: std::path::PathBuf },

    #[error("no usable PHP binary found (looked for: {0})")]
    PhpNotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;
