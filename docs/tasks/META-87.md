# META-87: каталог признаков и TI в PostgreSQL — `RiskStore` в `PgMeta`: пакеты, политика, источники, учёт

lane: meta
deps: [CTR-95]
class: A
files: [crates/tp-meta/*]
read: [docs/tasks/CTR-95.md, docs/CONTRACTS.md, docs/decisions/2026-11-11-stage2-followups.md#126]
checks: [nextest]
manual: no

## Сделать
- Миграция — номер выдаёт оркестратор.
- Пакеты поставщика: строка на версию — `version bigint` (первичный ключ), манифест `jsonb`, подпись `bytea`,
  `key_id`, ссылка на BLOB архива пакета, состояние (`active`, `previous`, `rejected`), отчёт lint и пробного
  прогона `jsonb`, время установки. Один `active` — частичным уникальным индексом; `put_package` с номером не выше
  максимального — ошибка ограничения (номер только растёт); очистка сверх `risk.package.keep` прошлых — в той же
  транзакции, что `activate`. Таблица — уровня установки, без `tenant`.
- Политика: строка на уровень (установка — `tenant IS NULL`, организация — уникальный `tenant`), изменения и свои
  признаки — `jsonb` по схеме CTR-95 с номером версии схемы; RLS по `tenant`, строку установки читают все.
- Источники TI: `ti_sources` (`id`, уровень, вид, формат, TTL, действие при устаревании, лицензия, `SecretRef` ключа —
  ссылка, не секрет; список организаций для MSSP) и `ti_list_versions` (`source`, `sha256`, ссылка на BLOB, число
  записей, `fetched_at`, `expires_at`); текущая версия — последняя по `fetched_at`. Сами записи списков — в BLOB, не
  в строках таблиц (миллионы IoC заказчика).
- Учёт MSSP: `ti_usage (source, tenant, day, lookups, hits)` — `record_usage` прибавляет (`ON CONFLICT … DO UPDATE`),
  RLS по `tenant`.
- Срабатывания: `risk_hits (tenant, message_ref, signal, mode, weight, score, at)` с индексом `(tenant, at)`; удаление
  старше `risk.hits.retention` — функцией для роли maintenance. RLS.
- `RiskStore` в `PgMeta` по контракту CTR-95; сведение уровней — функцией контракта, а не SQL.

## Как делают другие
- Пакеты правил с номером версии и хранением прошлых — sa-update (каналы, `--install`,
  https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html); карты отдельно от конфигурации — Rspamd
  (https://docs.rspamd.com/configuration/maps).
- Отличие: данные списков — в BLOB по версии с хэшем, в БД — только метаданные и учёт; откат — переключение строки.

## Приёмка
- Набор `tp-testkit` `suite::risk` проходит на `PgMeta` (PostgreSQL) наравне с памятью.
- Тест миграции: таблицы создаются поверх предыдущей, повтор ничего не меняет; второй `active` и пакет с меньшим
  номером — ошибки ограничений; при `keep = 2` после четвёртой установки хранятся активный и два прошлых.
- Чужая организация не читает политику, учёт и срабатывания другой (RLS); `record_usage` дважды за сутки — сумма.
