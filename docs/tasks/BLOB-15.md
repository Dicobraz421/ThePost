# BLOB-15: сборка мусора по учёту ссылок всех шардов

lane: blob
deps: [META-31, BLOB-14]
class: A
files: [crates/tp-blob/src/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S3.md#3.6, docs/spec/S3.md#3.4]
checks: [nextest]
manual: no

## Сделать
- Кандидат на удаление проверяется по учёту ссылок всех шардов справочника; окно W — без изменений.

## Приёмка
- Тест на in-memory учёте двух шардов: объект со ссылкой на одном шарде не удаляется.
