# CTR-43: очередь модерации — и для правил потока почты

lane: contracts
deps: [CTR-31, SMTP-22]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-admin/src/*, crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 9.

## Сделать
- `HeldMessage.group` — необязательное поле; у записи — список модераторов (учётные записи или группа модераторов)
  и источник задержки (группа рассылки или правило потока с именем правила).
- `Moderation::deliver` для письма без группы — доставка исходным получателям; общий набор testkit на оба случая.
- Реализации и вызовы — в том же PR (tp-meta: колонка допускает NULL + миграция — META-44; tp-admin: очередь модератора
  показывает источник).

## Приёмка
- Общий набор: задержка правилом, одобрение — доставка получателям, отказ — DSN отправителю.
