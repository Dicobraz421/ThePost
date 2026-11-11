# JMAP-24: JMAP Sieve — SieveScript (RFC 9661)

lane: jmap
deps: [CTR-46, SIEVE-02, JMAP-02]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:ietf:params:jmap:sieve` (расширения из SIEVE-02, лимиты `SieveStore`): `SieveScript/get`, `/set` (`blobId` текста,
  `onSuccessActivateScript`, `onSuccessDeactivateScript`), `/query`, `SieveScript/validate`; ошибки проверки — `invalidSieve` с позицией.

## Приёмка
- Тесты на in-memory: загрузка через upload, активация, удаление активного — `scriptIsActive`, неверный скрипт — `invalidSieve`.
