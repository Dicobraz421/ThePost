# ADM-33: один API доменов: удалить `/orgs/{org}/domains`

lane: admin
deps: [UI-16]
class: B
files: [crates/tp-admin/src/*, crates/thepost/src/services.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Transport]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 2; решение 94.

Класс B: удаляется тип, который подставляет узел, — место вызова в `crates/thepost/src/services.rs` правится в том же PR
(межполосная по устройству, как контракт с вызовами — решение 71); ветка `x/…`.

## Сделать
- Удалить маршруты `GET/POST /api/v1/orgs/{org}/domains`, `DELETE …/domains/{id}`, временный трейт `DomainStore`,
  `domain_name` и поле сборщика; домены организации — только `/api/v1/orgs/{org}/transport/domains` (CTR-24).
- Проверка имени домена везде — `normalize_domain` (создание учётной записи, транспорт); OpenAPI без старых операций.
- `services.rs`: `PgDomains` (FIX-03) удалить.

## Приёмка
- Тесты tp-admin: старых маршрутов нет (404), учётная запись в непринятом домене — прежняя ошибка с подсказкой;
  интеграционный тест узла с `transport/domains` зелёный.
