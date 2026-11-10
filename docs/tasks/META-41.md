# META-41: слушатель сигналов и миграции при списке адресов СУБД

lane: meta
deps: [META-38]
class: A
files: [crates/tp-meta/src/*]
read: [docs/spec/S3.md#3.1]
checks: [nextest]
manual: no

Отчёт deploy-06 (DEP-09).

## Сделать
- С `pgbouncer: true` слушатель сигналов (LISTEN мимо PgBouncer) берёт прямые адреса из списка (`direct_urls`) с поиском
  primary, а не единственный `direct_url` — чтобы эталон Enterprise-ВМ мог стоять за PgBouncer (SRS 3.1).
- Миграции под `database.owner` — с поиском primary по списку адресов, а не только `url`.
- Тест при `TEST_DATABASE_URL`: два адреса, первый недоступен — слушатель и миграции на втором.
