# DEPS-03: зависимость cms для подписи профиля .mobileconfig

lane: deps
deps: [JMAP-14]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-jmap/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 3.

## Сделать
- RustCrypto `cms` (подпись SignedData) в workspace и в tp-jmap; проверка `cargo deny` (лицензии, дубликаты).

## Приёмка
- `cargo deny` и `cargo audit` зелёные; tp-jmap собирается.
