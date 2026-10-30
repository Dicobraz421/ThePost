# SRCH-07: Извлечение текста вложений

lane: search
deps: [SYS-04, SRCH-02]
class: A
files: [crates/tp-search/src/extract/*]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Подкоманда-экстрактор в пуле подпроцессов SYS-04 (SRS 2.7), сбой → документ «не извлечён»: PDF (lopdf), DOCX/PPTX (zip + quick-xml), XLSX/ODS (calamine), ODT, HTML, TXT; лимиты из конфигурации.

## Приёмка
- Тесты на вложениях корпуса; бомба-архив отклоняется.
