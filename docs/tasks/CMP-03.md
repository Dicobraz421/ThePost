# CMP-03: EmailSubmission/query и queryChanges

lane: jmap
deps: []
class: A
files: [crates/tp-jmap/src/*]
read: []
checks: [nextest]
manual: no

Находка CMP-01, п. 3: `EmailSubmission/query` — `unknownMethod`. Клиенту (Postman) метод нужен для отложенной отправки и
отмены. RFC 8621 §7.3.

## Сделать
- `EmailSubmission/query`: фильтры `identityIds`, `emailIds`, `threadIds`, `undoStatus`, `before`, `after` (по `sendAt`);
  сортировка `emailId`, `threadId`, `sentAt` (RFC 8621 §7.3); `position`, `anchor`, `limit`, `calculateTotal` — по RFC 8620 §5.5.
- `EmailSubmission/queryChanges` (RFC 8620 §5.6); неподдерживаемое — `cannotCalculateChanges`.
- Метод — в capability `urn:ietf:params:jmap:submission`; тесты на фейках `tp-testkit` (фильтр `undoStatus: pending`,
  `sendAt`, пустой результат, `queryState` меняется после `EmailSubmission/set`).
