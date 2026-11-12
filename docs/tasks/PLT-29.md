# PLT-29: проверка конфигурации: ACME вместе с `tls.key` — ошибка

lane: platform
deps: [PLT-24]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 40: app-32, PR #481. ACME пишет выпущенный ключ в `key_file`, а узел при `tls.key` читает ключ из
секрета — после продления пара разойдётся. Решение 80: ACME при `tls.key` запрещён — запрет не проверяется.

## Сделать
- `validate` секций `tls`/`acme`: `acme.enabled` вместе с `tls.key` — `Error::Invalid` с подсказкой (ключ в хранилище —
  выпуск через PKI OpenBao, PLT-26).

## Приёмка
- Тест: конфигурация с ACME и `tls.key` не проходит `config check`, без `tls.key` — проходит.
