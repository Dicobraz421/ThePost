# META-85: политика доверия в PostgreSQL — `TrustStore` в `PgMeta`, внешний текст автоответа

lane: meta
deps: [CTR-93]
class: A
files: [crates/tp-meta/*]
read: [docs/tasks/CTR-93.md, docs/CONTRACTS.md, docs/decisions/2026-11-11-stage2-followups.md#124]
checks: [nextest]
manual: no

## Сделать
- Миграция — номер выдаёт оркестратор. Таблица политик доверия — по строке на уровень: установка (`tenant IS NULL`,
  единственность — частичным уникальным индексом), организация (`group_id IS NULL`, уникальный `tenant`), группа
  (уникальная пара `tenant`, `group_id`, ссылка на группу с каскадным удалением); клетки — `jsonb` по схеме CTR-93 с
  номером версии схемы, время изменения. RLS по `tenant`, строку установки читают все организации.
- Таблицы доменов-партнёров и доменов автопересылки организации: `(tenant, domain)` уникально, домен в нижнем
  регистре (`CHECK`), RLS.
- Внешний текст автоответа (`Vacation::external_*`, CTR-93) — колонки к хранилищу автоответа, `NULL` по умолчанию.
- `TrustStore` в `PgMeta` по контракту CTR-93: `effective_for` — один запрос (установка, организация, группы учётной
  записи через членство), разбор и сведение — функцией контракта, а не SQL; `partner_of` — по индексу домена.

## Как делают другие
- Политика хранится отдельно от объектов и наследуется уровнями: Exchange — Sharing Policy и Organization Relationship
  как отдельные объекты (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-sharingpolicy);
  Google — настройки OU и групп (https://knowledge.workspace.google.com/admin/calendar/set-google-calendar-sharing-options).
- Отличие: клетки в `jsonb` с проверкой контрактом при записи — новое поле политики не требует миграции схемы клеток.

## Приёмка
- Набор `tp-testkit` `suite::trust` проходит на `PgMeta` (PostgreSQL) наравне с памятью.
- Тест миграции: таблицы создаются поверх предыдущей, повторный запуск ничего не меняет; вторая строка уровня
  установки и повтор домена-партнёра — ошибки ограничений; удаление группы удаляет её политику.
- Чужая организация не читает политику и партнёров другой (RLS).
