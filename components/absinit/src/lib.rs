//! absinit: Absolution's service manager and init.
//!
//! Loading ([`config`]) and ordering ([`plan`]) produce the services to run;
//! the [`supervisor`] decides what to do with them as events arrive, without
//! touching the system; the [`runtime`] carries out those decisions with
//! processes, pipes and signals.

pub mod config;
pub mod plan;
pub mod runtime;
pub mod supervisor;
