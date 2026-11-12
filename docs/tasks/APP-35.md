# APP-35: `tls.key` из OpenBao в работающем узле и `doctor`

lane: app
deps: [PLT-24]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

Хвост волны platform-14, PR #423.

## Сделать
- `serve.rs`: `TlsCerts::load(&tls)` → `TlsCerts::load_with_secrets(&tls, store)` — хранилище секретов узла подключается
  до загрузки сертификатов. Сейчас `tls.key` даёт ошибку старта «the secret store is not connected» (решение 37).
- `doctor.rs`, `doctor_dns.rs`: при `tls.key` файл `key_file` не проверяется; вместо него — секрет читается и ключ
  подходит к цепочке (значение не выводится).

## Приёмка
- Тест узла: `tls.key` из `MemSecretStore` — слушатель с TLS стартует, новая версия секрета подхватывается;
  `doctor` с `tls.key` без `key_file` не сообщает об отсутствующем файле.
