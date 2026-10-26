//! Tests for the configuration model, grouped as `mod.rs` groups the modules.

use super::*;

mod access_log;
mod acme;
mod endpoints;
mod grpc;
mod grpc_proto;
mod metrics;
mod parse;
mod relay;
mod routes;
mod rpc;
mod sampling;
mod services;
mod sites;
mod static_files;
mod storages;
mod tls;
mod trace_context;
mod upstream;
