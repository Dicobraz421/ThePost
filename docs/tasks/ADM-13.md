# ADM-13: Serialize и Deserialize у структур Admin API

lane: admin
deps: [ADM-12]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 15.

## Сделать
- Структуры запросов (`NewAccount`, `NewAppPassword`, `DomainBody`, `AccountQuery`, `VersionQuery`) — `Serialize`;
  ответов (`AdminSession`, `WhoAmI`, `AccountInfo`, `QuotaInfo`, `Policies`, `AppPasswordInfo`, `IssuedAppPassword`,
  `DomainList`, `DomainWritten`, `Written`, `Problem`) — `Deserialize`; поля и вложенные типы — `pub`, если нужно клиенту.
- Тест: сериализация → разбор той же структурой даёт то же значение.

## Приёмка
- Структуры читаются и пишутся в обе стороны; OpenAPI без изменений.
