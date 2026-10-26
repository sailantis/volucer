//! Configuration model.
//!
//! The shape is one or two listeners, a set of named backends, a route table selecting between
//! them, and an optional static-content layer in front of the whole thing.
//!
//! This module is the model's index: it declares the submodules, re-exports what they make public,
//! and holds the two defaults that more than one table names. Everything else lives in the
//! submodule named after the table it reads.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ratelimit::KeyMode;

mod attach;
mod backend;
mod grpc;
mod grpc_proto;
mod listener;
mod metrics;
mod paths;
mod rpc;
mod rules;
mod server;
mod services;
mod sites;
mod spawn_cgi;
mod static_files;
mod tls;
mod top;
mod upstream;
mod validate;
mod validate_acme;
mod validate_backend;
mod validate_sites;
mod validate_tls;
mod worker;

pub use attach::*;
pub use backend::*;
pub use grpc::*;
pub use listener::*;
pub use metrics::*;
pub use paths::*;
pub use rpc::*;
pub use server::*;
pub use services::*;
pub use sites::*;
pub use spawn_cgi::*;
pub use static_files::*;
pub use tls::*;
pub use top::*;
pub use upstream::*;
pub use worker::*;

const DEFAULT_WORKERS: u32 = 0; // 0 = number of logical CPUs

/// The two defaults that more than one table names, so they cannot live in a single table's module.
///
/// Each child module reaches them through `use super::*`, which is what makes
/// `#[serde(default = "default_workers")]` resolve from `worker.rs` and `spawn_cgi.rs` alike.
fn default_workers() -> u32 {
    DEFAULT_WORKERS
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests;
