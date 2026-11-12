# ADM-20: группы — имена участников одним ответом и точная схема `GroupInfo`

lane: admin
deps: []
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны console-02, PR #439.

Подтверждено владельцем — решение 73 (`docs/decisions/2026-11-11-stage2-followups.md`).

## Сделать
- `GroupInfo` (`groups.rs`): у участников, перечисленных отправителей и модераторов вместе с идентификатором —
  имя входа учётной записи или имя группы; имена читаются пакетно, а не запросом на участника. Тело `POST`/`PATCH`
  группы не меняется.
- Точные схемы OpenAPI вместо `#[schema(value_type = Object)]`: `GroupMember` — `oneOf` `{account}` / `{group}`;
  `SenderRestriction` — `oneOf` строк `"anyone"`, `"internal"` и объекта `{only: {accounts, groups}}`.
- После мержа консоль убирает обход (`components/account-select.tsx`, разбор в `pages/groups.tsx`) — карточка полосы console.

## Приёмка
- Тест роутера: группа из 200 участников — имена в одном ответе; тест схемы: `members` и `senders` — не
  произвольный объект; `operations_match_openapi` в `thepost` остаётся зелёным.
