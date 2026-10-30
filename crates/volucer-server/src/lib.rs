//! Backend supervision and dispatch.
//!
//! One [`Backend`] per configured backend, one [`Dispatcher`] in front of them,
//! so both front-ends have a single call to make.

pub mod backend;
pub mod build;
pub mod calls;
pub mod dispatch;
pub mod error;
pub mod frontend;
pub mod jobs;
pub mod lifecycle;
pub mod listener;
pub mod params;
pub mod pool;
pub mod respawn;
pub mod rpc;
pub mod upstream_pool;
pub mod worker_pool;

pub use backend::{Backend, Contention};
pub use build::{MINIMUM_PHP_VERSION, VERSION, WORKER_PROTOCOL_VERSION};
pub use calls::{
    ConfigNamespace, KvNamespace, MethodRegistry, Namespace, PoolLookup, ResetRefusal,
    WorkerStatus, WorkersNamespace,
};
pub use dispatch::Dispatcher;
pub use dispatch::PoolDirectory;
// For `serve` to declare a deployment's `[metrics.collect]` names in the process registry before the
// listener exists, and for the pool tests to call the same function rather than copy it.
pub use dispatch::describe_collected;
pub use error::{Result, ServerError};
pub use frontend::fastcgi::{self, IncomingRequest};
pub use jobs::Consumers;
pub use lifecycle::LifecycleGuard;
pub use listener::{FastCgiListener, Limits, ShutdownHandle, Stats};
pub use params::{decode_base64, encode_base64, Params};
pub use pool::CgiPool;
pub use respawn::{RespawnReport, Unmanaged};
pub use rpc::{new_secret, RpcGrants, RpcListener, RpcStats, RpcStopHandle, Token, TokenHandler};
pub use upstream_pool::Supervisor;
pub use worker_pool::WorkerPool;
