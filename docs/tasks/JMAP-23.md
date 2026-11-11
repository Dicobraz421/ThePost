# JMAP-23: VacationResponse — автоответ из JMAP

lane: jmap
deps: [CTR-46, JMAP-02]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:ietf:params:jmap:vacationresponse`, `VacationResponse/get` и `/set` (RFC 8621 §8, `id` = `singleton`) поверх
  `SieveStore::vacation`/`set_vacation`; тип push `VacationResponse` — по событию `SieveChanged` (CTR-46).
- Postman и другие JMAP-клиенты включают автоответ без ManageSieve.

## Приёмка
- Тесты на in-memory: включение с датами, выключение, `/set` с неверным `id` — `notUpdated`.
