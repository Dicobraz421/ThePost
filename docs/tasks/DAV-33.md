# DAV-33: импорт 8-битных vCard (2.1 и 3.0 с CHARSET без ENCODING)

lane: dav
deps: []
class: A
files: [crates/tp-dav/src/*]
read: [docs/tasks/KIT-10.md]
checks: [nextest]
manual: no

## Сделать
- `calcard` принимает только `&str` и учитывает `CHARSET` лишь вместе с `ENCODING`: vCard с сырыми 8-битными байтами
  (выгрузки старых телефонов и Outlook, windows-1251) сервер не принимает (отчёт testkit-07, #639).
- Перед разбором в CardDAV: тело не UTF-8 — перекод по `CHARSET` свойства/карточки (готовая библиотека `encoding_rs`,
  уже в дереве зависимостей), затем `calcard`.

## Приёмка
- Тест на корпусе `tp-testkit` (`16-v21-cp1251-8bit`): PUT карточки сохраняет кириллицу верно, поиск её находит.
