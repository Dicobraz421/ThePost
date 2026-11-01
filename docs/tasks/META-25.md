# META-25: поиск primary и поведение при переключении СУБД

lane: meta
deps: [META-02]
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S3.md#3.1]
checks: [nextest]
manual: no

## Сделать
- Список адресов в секции PostgreSQL; выбор доступного для записи (семантика `target_session_attrs=read-write`).
- Признаки переключения: обрыв, `25006`, `57P01` — сброс пула, поиск primary, повтор с нарастающей паузой (параметры в секции).
- Повтор только идемпотентных операций; исход COMMIT при обрыве — по ключу операции.
- Метрики: длительность переключения, число повторов.

## Приёмка
- Тест на двух экземплярах PostgreSQL (при `TEST_DATABASE_URL`): перевод primary в read-only — узел переключается без потери записей.
