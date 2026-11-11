# DEPS-04: зависимости крейтов Ф2 — tp-dav, tp-sieve, tp-jmap

lane: deps
deps: [CTR-45, CTR-46]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-dav/Cargo.toml, crates/tp-sieve/Cargo.toml, crates/tp-jmap/Cargo.toml]
read: [docs/spec/S7.md#7.4, docs/decisions/2026-10-31-ready-made-first.md]
checks: [nextest]
manual: no

Новых библиотек нет: календари и контакты — `calcard` (уже в workspace), XML — `quick-xml`, письма — `mail-builder`, `mail-parser`.

## Сделать
- tp-dav: `async-trait`, `base64`, `futures`, `mail-builder`, `mail-parser`, `serde`, `time`, `tokio`; dev — `insta`, `tower` (`util`).
- tp-sieve: `tp-platform` (слушатель и TLS ManageSieve), `async-trait`, `base64`, `futures`, `mail-builder`, `serde`, `time`, `tokio`,
  `tokio-util`, `tracing`; dev — `insta`, `proptest`.
- tp-jmap: `calcard` (JSCalendar, JSContact), разрешённые крейты `tp-dav` (индекс объектов, iTIP) и `tp-sieve` (проверка скриптов JMAP Sieve).
- Цели cargo-fuzz крейтов — в их полосах (`crates/*/fuzz`, CI находит сам).

## Приёмка
- `cargo deny` и `cargo audit` зелёные; три крейта собираются.
