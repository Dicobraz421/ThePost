# ADM-16: Admin API — ресурсы, представители, общий доступ

lane: admin
deps: [CTR-48]
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2, docs/spec/S4.md#4.4, docs/spec/APP-I.md]
checks: [nextest]
manual: no

## Сделать
- Ресурсы: создание учётной записи-ресурса и `BookingPolicy` (`/api/v1/orgs/{org}/resources`), список, изменение, удаление.
- Представители учётной записи (`…/accounts/{account}/delegates`), права на календари и адресные книги (`…/collections/{id}/acl`),
  политика общего доступа организации (`/api/v1/orgs/{org}/calendar-sharing`).
- Схемы в OpenAPI, аудит.

## Приёмка
- Тесты на in-memory; OpenAPI содержит операции.
