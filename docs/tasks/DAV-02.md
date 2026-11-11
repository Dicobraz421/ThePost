# DAV-02: PROPFIND, PROPPATCH, OPTIONS и XML multistatus

lane: dav
deps: [DAV-01]
class: A
files: [crates/tp-dav/src/*, crates/tp-dav/fuzz/*]
read: [docs/spec/S6.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Разбор тел запросов `quick-xml` (пространства имён `DAV:`, `urn:ietf:params:xml:ns:caldav`, `…:carddav`,
  `http://calendarserver.org/ns/`, `http://apple.com/ns/ical/`) с лимитами глубины и размера; DTD и внешние сущности не обрабатываются.
- `PROPFIND` (`Depth: 0|1`, `infinity` — 403 `propfind-finite-depth`; `prop`, `allprop`, `propname`), `PROPPATCH` (`displayname`,
  описание, цвет, порядок — атомарно, 424 для остальных при ошибке одного), `OPTIONS` с `DAV: 1, 3, access-control, calendar-access,
  addressbook, extended-mkcol`.
- Формирование `multistatus` (207), ошибки с элементами предусловий (RFC 4918 §16), `resourcetype`, `getetag`, `getcontenttype`,
  `supported-report-set`, `current-user-privilege-set` (только свои права).
- Цель cargo-fuzz `xml` (разбор тел запросов).

## Приёмка
- Снимки (`insta`) ответов на запросы, записанные с Thunderbird, iOS и DAVx5; неизвестное свойство — 404 в `propstat`.
