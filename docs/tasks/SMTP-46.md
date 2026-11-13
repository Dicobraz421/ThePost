# SMTP-46: адрес клиента — без нормализации в потребителях; `XCLIENT`/`XFORWARD ADDR` — в канонической форме

lane: smtp
deps: [PLT-34]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/tasks/PLT-34.md]
checks: [nextest]
manual: no

PLT-34 (#707) приводит адрес соединения к канонической форме при приёме (`Listener::accept`, PROXY v2): IPv4,
отображённый в IPv6 (`::ffff:a.b.c.d`), приходит адресом IPv4. Вызовы `to_canonical` в потребителях стали вторым
путём; правило владельца — второй путь не оставлять. Адрес `ADDR` из `XCLIENT`/`XFORWARD` через слушатель не
проходит — его каноническая форма задаётся при разборе.

## Сделать
- `sources::parse`: `ADDR` — в канонической форме (`IPV6:::ffff:a.b.c.d` — адрес IPv4 `a.b.c.d`).
- Убрать `to_canonical` из `authcheck.rs` (SPF), `deliver.rs` (`Received`), `milter/mod.rs` (макросы),
  `screen.rs` (`admit_at`, `record_at`), `sources.rs` (`Sources::resolve`).
- Оставить приведение одиночного адреса `screen.exempt` (из конфигурации) в `screen.rs`.
- Тесты: `screen/tests.rs`, `sources/tests.rs` — адреса IPv4 без формы `::ffff:`; разбор `ADDR=IPV6:::ffff:…`.

## Приёмка
- В `tp-smtp` `to_canonical` — только у `ADDR` в `sources::parse` и у `screen.exempt`.
- `XCLIENT ADDR=IPV6:::ffff:192.0.2.1` — адрес `192.0.2.1`; `XFORWARD ADDR=2001:db8::1` — без изменений.
