# CTR-68: `InternalTls`: ключ только из хранилища секретов

lane: contracts
deps: [CTR-52, AUTH-17]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/CTR-52.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 8; решение 100.

## Сделать
- `InternalTls.key_file` и `KeySource::File` удалить, только `key: { secret }` (CTR-52; хвост реестра, п. 79); места других крейтов, где поле собирается
  перечислением, — в этом же PR (класс B); `docs/CONTRACTS.md#Internal TLS`, CHANGELOG.

## Приёмка
- CI зелёный; `key_file` в YAML — ошибка разбора.
