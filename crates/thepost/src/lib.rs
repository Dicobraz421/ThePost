//! The Post: сборка ролей узла по config.yaml (SRS 2.1), команды `serve`, `config`, `doctor`, `verify`, `admin`.

pub mod acme;
pub mod admin_client;
pub mod audit_export;
pub mod cli;
pub mod doctor;
pub mod doctor_dns;
pub mod infra;
pub mod local_admin;
pub mod log_reader;
pub mod metrics;
pub mod modules;
pub mod oidc_device;
pub mod openapi;
pub mod profile_signer;
pub mod protocols;
pub mod roles;
pub mod secrets;
pub mod sections;
pub mod serve;
pub mod services;
pub mod startup;
pub mod verify;

#[cfg(test)]
mod testutil;
