# CTR-16: конверт письма и пересылка в контракте ingest

lane: contracts
deps: [CTR-05]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-31-ingest-envelope.md]
checks: [nextest]
manual: no

## Сделать
- `IngestRequest.mail_from`; результат `ingest` — `forward: Vec<адрес>` (копии на внешние адреса от `DecisionHook`).
- Вызовы в tp-mailstore и tp-testkit поправить в том же PR (ветка `x/contracts-06`).

## Приёмка
- Тесты контрактов зелёные; in-memory реализация testkit заполняет `forward`.
