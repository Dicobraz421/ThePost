//! CalDAV, CardDAV, scheduling, free/busy, GAL (SRS 4.4).
//!
//! Основа (DAV-01…04): маршруты под `/dav/` ([`router`]), вход Basic и Bearer, свойства (`PROPFIND`,
//! `PROPPATCH`, RFC 4918), ресурсы (`GET`, `PUT`, `DELETE`, `MKCALENDAR` RFC 4791, расширенный `MKCOL` RFC 5689),
//! синхронизация (`REPORT sync-collection` RFC 6578, `getctag`), отчёты CalDAV (`calendar-query`,
//! `calendar-multiget`, `free-busy-query`, RFC 4791 §7.8–7.10), отчёты CardDAV (`addressbook-query`,
//! `addressbook-multiget`, RFC 6352 §8.6–8.7). Хранилище — [`tp_contracts::dav::DavStore`].
//!
//! Глобальная адресная книга «Организация» только для чтения — в доме адресных книг каждого пользователя: хранится
//! в `DavStore` (CTR-60), строит её из каталога [`gal::sync`].
//!
//! Планирование встреч (DAV-13…16, RFC 6638; [`router_with`] с [`ScheduleDeps`]): рассылка iTIP после `PUT` и
//! `DELETE`, `schedule-inbox` и `schedule-outbox`, `Schedule-Tag`, запрос занятости.
//!
//! Общий доступ (DAV-17…19; [`router_shared`] с [`SharingDeps`]): чужие календари и адресные книги по правам
//! `SharingAuthorizer` (CTR-84), выдача прав `POST` (CalendarServer sharing), представители `calendar-proxy`, поиск
//! принципалов и переговорных; бронирование ресурсов при приёме iTIP — [`ItipInbox::with_booking`].
//!
//! - [`config`] — секция `dav` ([`DavConfig`]);
//! - [`gal`] — книга GAL из каталога ([`gal::sync`], [`GalDeps`]);
//! - [`itip`] — разбор и построение iTIP (RFC 5546), общий с `tp-jmap`;
//! - [`deliver`] — приём iTIP учётной записью ([`ItipInbox`] — реализация `ItipDelivery`);
//! - [`imip`] — письмо iMIP внешним участникам ([`ImipSender`] — реализация `ItipSender`);
//! - [`schedule`] — рассылка ([`ScheduleDeps::dispatch`]) и ресурсы планирования;
//! - [`index`] — индекс объекта (UID, компонент, интервал с повторами) из тела, им пользуется и `tp-jmap`;
//! - [`charset`] — перекод 8-битной vCard (`CHARSET` без `ENCODING`) в UTF-8 перед calcard, общий с `tp-jmap`;
//! - [`xml`] — разбор XML-тел с лимитами и запись `multistatus`;
//! - [`fuzz`] — проверки разбора для цели cargo-fuzz `crates/tp-dav/fuzz` (не API).

mod alarm;
pub mod booking;
mod card;
pub mod charset;
pub mod config;
pub mod deliver;
pub mod fuzz;
pub mod gal;
pub mod imip;
pub mod index;
pub mod itip;
mod path;
mod principals;
mod props;
mod proxy;
mod query;
mod resource;
pub mod schedule;
mod server;
mod share;
mod sync;
pub mod xml;

#[cfg(test)]
mod tests;

pub use booking::BookingDeps;
pub use config::DavConfig;
pub use deliver::ItipInbox;
pub use gal::GalDeps;
pub use imip::ImipSender;
pub use path::PREFIX;
pub use schedule::ScheduleDeps;
pub use server::{ALLOW, DAV_HEADER, DAV_HEADER_SHARING, Deps, router, router_shared, router_with};
pub use share::SharingDeps;
