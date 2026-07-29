//! bizik — mark folders on the machines you work on, then launch and watch
//! agent sessions in all of them from one screen.
//!
//! The same executable plays both roles. On a server it answers host-side
//! commands; on a laptop it runs the dashboard and drives servers over SSH.

#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

mod agent;
mod application;
mod attention;
mod cli;
mod hooks;
mod hostenv;
mod hostops;
mod model;
mod probe;
mod reconcile;
mod remote;
mod sidebar;
mod store;
mod tmux;
mod tui;
mod util;

pub use cli::run;
