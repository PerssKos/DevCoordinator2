//! Rust control plane for DevCoordinator2.

pub mod access;
pub mod alerts;
pub mod artifact_materialize;
pub mod bugs;
pub mod capacity;
pub mod check_event;
pub mod cli;
pub mod client;
pub mod config;
pub mod control_plane;
pub mod daemon;
pub mod database;
mod delivery;
mod deployment_cancellation;
pub mod deployment_files;
pub mod deployment_git;
pub mod deployment_health;
pub mod deployment_state;
pub mod deployments;
pub mod docker;
pub mod events;
pub mod glossary;
pub mod health;
pub mod ids;
pub mod inventory;
pub mod mcp;
pub mod metrics;
pub mod metrics_sampler;
pub mod metrics_source;
pub mod plan;
pub mod platform;
pub mod ports;
pub mod progress;
pub mod repository;
pub mod repository_config;
mod review;
mod review_validation;
pub mod routes;
pub mod runtime_configuration;
mod socket_endpoint;
pub mod systemd;
pub mod telegram;
pub mod test_admission;
pub mod test_artifacts;
pub mod test_command;
pub mod test_evidence;
pub mod test_lifecycle;
pub mod test_logs;
pub mod test_state;
pub mod usage;

#[cfg(test)]
mod automation_test_support;

pub use devcoordinator2_api::DATABASE_SCHEMA_VERSION;
pub const SOURCE_COMMIT: &str = match option_env!("DEVCOORDINATOR2_SOURCE_COMMIT") {
    Some(value) => value,
    None => "development",
};
