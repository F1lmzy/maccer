//! Maccer launcher.
//!
//! The launcher, service, native frontends, and providers live in this single
//! package. Module boundaries mirror the former workspace crates:
//! [`core`] (search/ranking), [`protocol`] (service DTOs), [`client`] (IPC
//! client), [`config`], [`macos`] (native integration), [`agent`] (C ABI and
//! service frontend), [`service`] (backend), [`ui`] (GPUI frontend), and
//! [`providers`].

pub mod agent;
pub mod client;
pub mod config;
pub mod core;
pub mod macos;
pub mod protocol;
pub mod providers;
pub mod service;
pub mod ui;
