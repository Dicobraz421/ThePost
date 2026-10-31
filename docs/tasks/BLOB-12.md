# BLOB-12: убрать совместимые поля конфига tp-blob

lane: blob
deps: [BLOB-11, APP-07]
class: A
files: [crates/tp-blob/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- Удалить оставленное ради `thepost` до APP-07 (PR #100): `LocalConfig.stale_tmp_secs`, `S3Config.*_ms`, поля значений ключей и `S3Store::new` с ключами.

## Приёмка
- В tp-blob нет старых имён; `thepost` собирается без правок.
