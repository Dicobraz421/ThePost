//! CalDAV, CardDAV, scheduling, free/busy, GAL (SRS 4.4).
//!
//! Основа (DAV-01…04): маршруты под `/dav/` ([`router`]), вход Basic и Bearer, свойства (`PROPFIND`,
//! `PROPPATCH`, RFC 4918), ресурсы (`GET`, `PUT`, `DELETE`, `MKCALENDAR` RFC 4791, расширенный `MKCOL` RFC 5689),
//! синхронизация (`REPORT sync-collection` RFC 6578, `getctag`), отчёты CalDAV (`calendar-query`,
//! `calendar-multiget`, `free-busy-query`, RFC 4791 §7.8–7.10), отчёты CardDAV (`addressbook-query`,
//! `addressbook-multiget`, RFC 6352 §8.6–8.7). Хранилище — [`tp_contracts::dav::DavStore`].
//!
//! Глобальная адресная книга «Организация» только для чтения из каталога ([`router_with_gal`]) — в доме адресных книг
//! каждого пользователя.
//!
//! - [`config`] — секция `dav` ([`DavConfig`]);
//! - [`index`] — индекс объекта (UID, компонент, интервал с повторами) из тела, им пользуется и `tp-jmap`;
//! - [`xml`] — разбор XML-тел с лимитами и запись `multistatus`;
//! - [`fuzz`] — проверки разбора для цели cargo-fuzz `crates/tp-dav/fuzz` (не API).

mod card;
pub mod config;
pub mod deliver;
pub mod fuzz;
mod gal;
pub mod index;
pub mod itip;
mod path;
mod props;
mod query;
mod resource;
mod server;
mod sync;
pub mod xml;

#[cfg(test)]
mod tests;

pub use config::DavConfig;
pub use deliver::ItipInbox;
pub use path::PREFIX;
pub use server::{ALLOW, DAV_HEADER, Deps, GalDeps, router, router_with_gal};
