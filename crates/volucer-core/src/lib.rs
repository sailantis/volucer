//! Shared building blocks for volucer: configuration, backend model and PHP discovery.

pub mod clientip;
pub mod config;
pub mod env;
pub mod error;
pub mod hostmap;
pub mod php;
pub mod preflight;
pub mod program;
pub mod ratelimit;
pub mod script;
pub mod sites;
pub use clientip::{ClientIpPolicy, HeaderLookup};
pub use config::{BackendConfig, Config, Listener, ServerConfig};
pub use error::{Error, Result};
pub use hostmap::HostMap;
pub use php::{PhpBinary, Sapi};
pub use preflight::{inspect, Warning};
pub use program::{resolve_program, split_command};
pub use ratelimit::{Identity, KeyMode, RateLimitPolicy, Reason, Verdict};
pub use script::{ScriptExtensions, DEFAULT_SCRIPT_EXTENSIONS};
pub use sites::{ResolvedSite, Sites};
