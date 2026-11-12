# APP-36: `InternalTls.key` в узле — межузловой канал и СУБД

lane: app
deps: [CTR-52, CLU-16, META-56, APP-35]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Internal TLS, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

Хвост волны platform-14, PR #423.

Требует подтверждения владельца: доводить ключ из OpenBao до межузлового канала и клиента к СУБД (вариант А агента)? (п. 18 синка 12.11)

## Сделать
- `sections.rs`: проверка `cluster.rpc.tls` — `key_file` либо `key`; `startup.rs`: при `key` файл ключа не входит в
  пути доступа роли; хранилище секретов передаётся в `NodeTls::load_with_secrets` (CLU-16) и пулу СУБД (META-56).
- `doctor`: для `cluster.rpc.tls.key` и `database.tls.key` — то же правило, что в APP-35.

## Приёмка
- Тест: узел с `cluster.rpc.tls.key` и `database.tls.key` из `MemSecretStore` стартует; закрытых ключей на диске нет.
