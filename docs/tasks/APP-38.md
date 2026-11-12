# APP-38: подключения хвостов Ф2 в узле — адреса для CalDAV, темы писем, пределы Sieve в JMAP

lane: app
deps: [DAV-23, META-57, ADM-19, JMAP-35, APP-29, APP-27]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волн dav-02 (PR #427), console-02 (PR #439) и app-25 (PR #442).

## Сделать
- `tp-dav`: при сборке сервера передать `AccountStore` построителем из DAV-23 — псевдонимы в `calendar-user-address-set`.
- `tp-admin`: если ADM-19 ввела построитель источника заголовков письма (тема в корзине и очереди модерации) —
  вызвать его в сборщике Admin API (`services.rs`); иначе пункт не нужен.
- Sieve: в `tp-jmap` передаётся проверка скрипта с пределами секции `sieve` (JMAP-35) — та же, что уже
  передана в Admin API (PR #442).

## Приёмка
- Интеграционный тест узла: `PROPFIND` принципала отдаёт адрес-псевдоним; `GET …/moderation` с запросом тем отдаёт
  тему письма; `SieveScript/validate` отклоняет скрипт сверх предела секции.
