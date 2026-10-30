# META-19: перечисление учёта BLOB для verify

lane: meta
deps: [META-04]
class: A
files: [crates/tp-meta/src/*]
read: [docs/spec/S3.md#3.5]
checks: [nextest]
manual: no

## Сделать
- Постраничное перечисление записей учёта BLOB и пометка повреждённых — то, что нужно `thepost verify` (APP-03); сейчас на рабочей БД verify завершается явной ошибкой.

## Приёмка
- Тест: verify на БД с внесённым расхождением находит и помечает его.
