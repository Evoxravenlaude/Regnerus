//! Regnerus: a watcher and a trader in one process.
//!
//! It follows traders as entities, not addresses. The watcher reads what
//! watched wallets do; the trail engine and the clusters work out which
//! other wallets are the same trader; the trader copies the entities that
//! have shown a delayed copy of them makes money.

pub mod app;
pub mod bot;
pub mod chain;
pub mod cluster;
pub mod config;
pub mod consensus;
pub mod identity;
pub mod logger;
pub mod model;
pub mod obs;
pub mod pipeline;
pub mod review;
pub mod runtime;
pub mod store;
pub mod trade;
pub mod trail;
pub mod util;
