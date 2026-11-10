# CTR-42: хранилище отправок JMAP (отложенная отправка и отмена)

lane: contracts
deps: [CTR-28]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Решение владельца #195: отложенная отправка и отмена — полностью.

## Сделать
- `SubmissionStore`: создать отправку (`sendAt`, `undoStatus: pending`), перечислить с фильтрами (RFC 8621 §7.3),
  отменить (`pending` → `canceled`), отметить отправленной (`final`), изменения для `queryChanges`; срок хранения.
- `Outbound::submit` с `sendAt` — постановка в очередь не раньше срока. In-memory и наборы; CHANGELOG.
- Далее волны: meta (таблица, RLS), smtp (очередь по `sendAt`, отмена до отправки), jmap (`maxDelayedSend` из
  конфигурации, `EmailSubmission/get|set|query|queryChanges` по хранилищу).
