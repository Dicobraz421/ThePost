//! The Post: сборка ролей узла по config.yaml (SRS 2.1), команды `serve`, `config`, `doctor`, `verify`.

pub mod cli;
pub mod doctor;
pub mod doctor_dns;
pub mod infra;
pub mod metrics;
pub mod modules;
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
