//! REST API module for external integrations
//!
//! Provides an HTTP API for querying and managing StellarNodes.

mod dto;
mod handlers;
mod server;
mod custom_metrics;

pub use server::run_server;
