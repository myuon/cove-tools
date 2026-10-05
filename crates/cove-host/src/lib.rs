//! `cove-host`: many small Cove web apps on one machine.
//!
//! Each directory under `apps/` holding an `app.toml` is an app. The host
//! loads every app once ([`apps`]): checks it against the host's schemas,
//! refuses it if its entry requires a capability it is not granted or its
//! code can `spawn`, lowers and prepares it, and compiles it on the native
//! tier where this machine has one. A request to `/<app>/...` is admitted
//! into that app's queue by the HTTP front ([`server`], hyper on tokio), and a
//! shared pool of worker threads ([`sched`]) runs it in a fresh `OwnedVm`:
//! parked at a host call that answers pending, yielded at a safepoint when
//! its slice is up and others wait, resumed on any worker, apps served round
//! robin.
//!
//! An app is updated in place ([`admin`], [`server::Host::update`]): the
//! next version is loaded off the request path and routed to only if it
//! loads; requests admitted to the old version finish on it, and it is
//! dropped with its last one ([`sched::Slot`]). [`ops`] is what the host
//! shows of all this.
//!
//! The README is the walkthrough; this crate's tests start the host
//! in-process on a free port.

pub mod admin;
pub mod apps;
pub mod config;
pub mod convert;
pub mod fetch;
pub mod hosts;
pub mod kv;
pub mod logs;
pub mod ops;
pub mod router;
pub mod sched;
pub mod server;
pub mod stats;
pub mod sys;
pub mod toolchain;

pub use apps::{App, AppState, Backend, LoadOptions};
pub use hosts::{HostModule, HostModules, PendingWork};
pub use router::{PathPrefix, Route, Router};
pub use server::{Host, ServeOptions, UpdateError};
