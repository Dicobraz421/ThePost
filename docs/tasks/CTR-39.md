# CTR-39: сигнал при снятии блокировки; пометка Membership

lane: contracts
deps: [CTR-23]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Отчёт meta-10 (META-27): `Blocked` публикуется только из `block`, `unblock` сигнала не даёт — узлы держат снятую
блокировку до её срока.

## Сделать
- `AbuseBlocks::unblock` публикует сигнал (`Blocked` с признаком снятия или отдельный вид `Unblocked { subject }`);
  in-memory и conformance-тест; CHANGELOG.
- CONTRACTS.md#Membership: убрать пометку «заглушка до META-27».
