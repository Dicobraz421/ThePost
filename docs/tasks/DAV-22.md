# DAV-22: оргструктура в глобальной адресной книге

lane: dav
deps: [DAV-12, CTR-35, AUTH-10]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/decisions/2026-11-04-exchange-parity.md]
checks: [nextest]
manual: no

## Сделать
- vCard GAL: `ORG` (организация; департамент; отдел), `TITLE`, руководитель (`RELATED;TYPE=x-manager`), почтовые контакты и
  динамические группы (CTR-35).

## Приёмка
- Тесты на in-memory каталоге с деревом подразделений; снимок карточки.
