# META-56: ключ клиента TLS к СУБД из хранилища секретов

lane: meta
deps: [CTR-52]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Internal TLS, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

Хвост волны platform-14, PR #423.

## Сделать
- `pool.rs`: при `database.tls.key` (CTR-52) ключ клиентского сертификата читается из `SecretStore` (он уже есть у пула —
  `credentials.rs`, `failover.rs`) и передаётся драйверу из памяти; на диск не пишется. `key_file` работает как раньше.
- Новая версия секрета применяется при новом подключении (как смена путей); недоступное хранилище или ключ, не
  подходящий к сертификату, — `Unavailable`/`Invalid` с понятным текстом без значения ключа; в логах ключа нет.
- То же для пулов шардов и соединения слушателя.

## Приёмка
- Тест: параметры подключения с `tls.key` из `MemSecretStore` содержат ключ из секрета; `key` без хранилища — ошибка
  конфигурации.
