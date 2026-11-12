# CTR-61: контракт хранилища сессий Admin API

lane: contracts
deps: [CTR-41]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 77 (этап 3): сессия Admin API видна всем узлам роли admin и переживает перезапуск.

## Сделать
- Трейт `AdminSessions` рядом с `LocalAdminTokens`: `put(hash, principal, roles, expires_at)`, `get(hash)` (истёкшая —
  `None`), `delete(hash)`, `purge_expired(now)`; ключ — хэш значения сессии (само значение не хранится).
- Реализация в памяти, общий набор тестов, `docs/CONTRACTS.md`, CHANGELOG. Потребители: META-64, ADM-28.

## Приёмка
- CI зелёный; тесты набора: сессия читается, истёкшая не читается, удаление и очистка.
