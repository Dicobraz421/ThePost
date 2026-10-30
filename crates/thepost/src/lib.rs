//! The Post: сборка ролей узла по config.yaml (SRS 2.1), команды `serve`, `config`, `doctor`, `verify`.

pub mod cli;
pub mod infra;
pub mod modules;
pub mod roles;
pub mod secrets;
pub mod sections;
pub mod serve;

#[cfg(test)]
mod testutil;
