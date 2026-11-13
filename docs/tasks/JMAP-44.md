# JMAP-44: импорт 8-битных vCard в ContactCard

lane: jmap
deps: []
class: A
files: [crates/tp-jmap/src/*]
read: [docs/tasks/DAV-33.md, docs/tasks/KIT-10.md]
checks: [nextest]
manual: no

## Сделать
- Тот же перекод, что DAV-33, при загрузке vCard через JMAP (ContactCard/parse, blob-импорт): тело не UTF-8 — перекод по
  `CHARSET`, затем `calcard`. Функцию перекода не дублировать: взять её из DAV-33 через общий крейт, от которого зависят
  `tp-dav` и `tp-jmap` (место — описать в PR).

## Приёмка
- Тест на корпусе `tp-testkit` (`15-v3-cp1251-qp`, `16-v21-cp1251-8bit`): кириллица верна.
