//! Текстовый интерфейс `thepost admin tui` в стиле Midnight Commander — тонкий клиент Admin API (решение 118).
//!
//! - [`app`] — состояние и клавиши: вход, две панели (дерево ресурсов и список/карточка), F1–F10 (TUI-01);
//! - [`ui`] — отрисовка 80×24 по утверждённому макету; [`theme`] — роли оформления (тема по токенам — TUI-06);
//! - [`schema`] и [`engine`] — общий движок списков, карточек и форм по схеме `openapi_spec()` (TUI-02);
//! - [`layout`] — ручные раскладки главных разделов поверх движка (TUI-03);
//! - [`node`] — конфигурация узла `config.yaml` на самом узле (TUI-04);
//! - [`cli`] — эквивалентная команда `thepost admin …` для действия на экране;
//! - [`term`] — запуск в терминале с клиентом `tp-admin-client` ([`run`]).
//!
//! Мышь — TUI-07.

pub mod app;
pub mod cli;
pub mod engine;
pub mod layout;
pub mod node;
pub mod schema;
pub mod term;
pub mod theme;
pub mod tree;
pub mod ui;

pub use node::{ConfigHost, NodeConfig};
pub use term::{Connect, run};
