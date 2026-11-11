//! CalDAV, CardDAV, scheduling, free/busy, GAL (SRS 4.4).
//!
//! Основа (DAV-01…04): маршруты под `/dav/` ([`router`]), вход Basic и Bearer, свойства (`PROPFIND`,
//! `PROPPATCH`, RFC 4918), ресурсы (`GET`, `PUT`, `DELETE`, `MKCALENDAR` RFC 4791, расширенный `MKCOL` RFC 5689),
//! синхронизация (`REPORT sync-collection` RFC 6578, `getctag`). Хранилище — [`tp_contracts::dav::DavStore`].
//!
//! - [`config`] — секция `dav` ([`DavConfig`]);
//! - [`index`] — индекс объекта (UID, компонент, интервал с повторами) из тела, им пользуется и `tp-jmap`;
//! - [`xml`] — разбор XML-тел с лимитами и запись `multistatus`;
//! - [`fuzz`] — проверки разбора для цели cargo-fuzz `crates/tp-dav/fuzz` (не API).

pub mod config;
pub mod fuzz;
pub mod index;
mod path;
mod props;
mod resource;
mod server;
mod sync;
pub mod xml;

#[cfg(test)]
mod tests;

pub use config::DavConfig;
pub use path::PREFIX;
pub use server::{ALLOW, DAV_HEADER, Deps, router};
