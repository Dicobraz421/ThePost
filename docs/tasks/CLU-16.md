# CLU-16: ключ mTLS межузлового канала из хранилища секретов

lane: cluster
deps: [CTR-52]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Internal TLS, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

Хвост волны platform-14, PR #423.

## Сделать
- `node_rpc.rs`: `NodeTls::load_with_secrets(tls, store)` — при `rpc.tls.key` (CTR-52) ключ читается из `SecretStore`
  и идёт в `NodeTls::from_pem`; на диск не пишется. `NodeTls::load` при заданном `key` — `Invalid` (как
  `TlsCerts::load`, PLT-24). Требование секции: `ca_file`, `cert_file` и `key_file` либо `key`.
- Перечитывание ключа — по тем же событиям, что и файлов пары сейчас; ключ, не подходящий к сертификату, — действует
  прежняя пара. Описание секции `rpc.tls` в `rpc.rs` дополнить.

## Приёмка
- Тест: два узла с ключом из `MemSecretStore` устанавливают mTLS-соединение; `key` и `key_file` вместе — ошибка.
