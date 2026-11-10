# META-40: повтор записей по ключу при переключении СУБД

lane: meta
deps: [META-38]
class: A
files: [crates/tp-meta/src/*]
read: []
checks: [nextest]
manual: no

Отчёт meta-21 (META-38): записи `PgMeta` выполняются один раз и при отказе отвечают `Unavailable`.

## Сделать
- На `with_tenant_keyed` (исход COMMIT по ключу): вставка письма по его id, `insert_delivery`, `hold`, `events.commit`.
- Повтор записей, идемпотентных по сути (upsert, удаление по id, `set_*` по значению) — список в коде с обоснованием.
- Тест при `TEST_DATABASE_URL`: обрыв после COMMIT — повтор не задваивает запись.
