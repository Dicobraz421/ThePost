# CTR-48: общий доступ к календарям и контактам, представители, ресурсы

lane: contracts
deps: [CTR-45, CTR-47]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.5, docs/spec/APP-I.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Этап 4, «рюшечки» Ф2: без этого календари и контакты владельца работают.

Решение владельца 2026-11-12 (PR #505): вид учётной записи — поле `Account::kind` (`AccountKind`, задаёт `AccountUpsert::kind`), а не
метод `ResourceStore::account_kind`; `tp-meta` (миграция 0026), `tp-testkit` и места сборки `Account` — в том же PR.

## Сделать
- `CollectionAcl`: записи прав на коллекцию (`read`, `write`, `free-busy`, `schedule`, `admin`) по субъектам, как у `AclStore`
  (имя входа, группа, `anyone`); `shared_with(scope) -> Vec<(owner, CollectionId, Rights)>` — для дома коллекций получателя.
- Представители: `Delegates { delegate, level: Read|Write, schedule_on_behalf }` учётной записи.
- Ресурсы (переговорные): `AccountKind::Resource` и `BookingPolicy { auto_accept, allow_conflicts, max_duration, horizon, bookers, working_hours }`;
  решение по приглашению — `BookingPolicy::decide(request, busy) -> Accept|Decline(reason)|Tentative`.
- Политика организации на общий доступ к календарям (APP-I «Политики общего доступа»): кому можно выдавать права, максимум прав вне группы.
- In-memory реализации и наборы testkit.

## Приёмка
- Conformance на in-memory; CHANGELOG в CONTRACTS.md.
