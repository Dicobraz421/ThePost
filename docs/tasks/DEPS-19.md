# DEPS-19: зависимости каталога признаков и TI — `ring` (Ed25519), `regex`, `publicsuffix`, `winnow`; `zip`, `infer`, `ipnet` в крейты

lane: deps
deps: []
class: B
files: [Cargo.toml, Cargo.lock, deny.toml, crates/thepost/Cargo.toml, crates/tp-smtp/Cargo.toml, crates/tp-sieve/Cargo.toml]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md]
checks: [deny]
manual: no

## Сделать
- Проверено при планировании: `ring` 0.17 уже в `Cargo.lock` (через `rustls`, `rustls-webpki`, `hickory-proto`) —
  Ed25519 подпись и проверка (`ring::signature::ED25519`, `Ed25519KeyPair`); прямая зависимость рабочего
  пространства той же версии, без нового пакета. `ed25519-dalek` не берём — это вторая криптобиблиотека и новые
  пакеты (`curve25519-dalek` и др.).
- `regex` 1 — уже в дереве транзитивно: прямой зависимостью рабочего пространства (`tp-sieve`, `tp-smtp`).
- `zip` и `infer` — уже в рабочем пространстве (`tp-search`, `tp-blob`): `zip` — в `tp-smtp` (флаг шифрования записей
  архива) и `thepost` (архив пакета), `infer` — в `tp-smtp` (тип вложения по содержимому). `ipnet` — уже в
  `Cargo.lock` транзитивно: прямой зависимостью рабочего пространства в `tp-smtp` (префиксы IP списков TI).
- `publicsuffix` 2.3 (MIT/Apache-2.0; список читается во время работы — обновляется пакетом) — новый, с
  `psl-types`; в `tp-smtp`. Сам список (MPL-2.0) — данные пакета, не зависимость: строка в `deny.toml` не нужна.
- STIX 2.1 — без крейта (`stix` 0.3 — 2021, `rstix`, `stix-rs` незрелые): `serde_json` уже в `thepost`, разбор
  подмножества шаблонов — `winnow` (уже в `Cargo.lock`) прямой зависимостью `thepost`. TAXII 2.1 — `reqwest` (есть).
- `thepost`: `ring`, `winnow`, `zip`. `cargo deny` проходит (лицензии, дубликаты); новых версий `ring`, `regex`,
  `winnow`, `ipnet` нет.

## Как делают другие
- Rspamd подписывает карты EdDSA (`rspamadm signtool`, https://docs.rspamd.com/configuration/maps); `regex` в Rust —
  линейное время на недоверенном вводе (https://docs.rs/regex/latest/regex/#untrusted-input). Готовое вместо своего
  (решение 2026-10-31): криптография и PSL — крейтами.

## Приёмка
- `cargo check -p thepost -p tp-smtp -p tp-sieve` (Linux и Windows) и `cargo deny` зелёные; в `Cargo.lock` по одной
  версии `ring`, `regex`, `winnow`, `ipnet`; новые пакеты — только `publicsuffix` и `psl-types`.
