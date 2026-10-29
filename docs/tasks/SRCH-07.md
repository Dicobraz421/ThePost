# SRCH-07: Извлечение текста вложений

lane: search
deps: [SYS-04, SRCH-02]
class: A
files: [crates/tp-search/src/extract/*]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Подкоманда-экстрактор в sandbox: PDF (lopdf), DOCX/PPTX (zip + quick-xml), XLSX/ODS (calamine), ODT, HTML, TXT; лимиты из конфигурации.

## Приёмка
- Тесты на вложениях корпуса; бомба-архив отклоняется.
