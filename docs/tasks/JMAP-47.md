# JMAP-47: матрица доверия в JMAP — принципалы и занятость своей организации, общий доступ, публикация, внешний автоответ

lane: jmap
deps: [CTR-93]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/tasks/CTR-93.md, docs/ADMIN-GUIDE.md#матрица-по-умолчанию]
checks: [nextest]
manual: no

## Сделать
- `Principal/get`, `Principal/query`, `Principal/getAvailability` (RFC 9670) и книга «Организация» (`ContactCard`) —
  только своя организация: принципалы соседей по установке не находятся и не отдаются.
- Общий доступ (`shareWith`, RFC 9670): только принципалы своей организации, иначе `invalidProperties`; уровень — не
  выше клетки `sharing` для `Own`.
- `Availability/get` (`urn:thepost:availability`, JMAP-19) и публикация (`urn:thepost:calendarpublish`, JMAP-20), когда
  появятся: уровень — не выше клеток `freebusy` и `sharing` для типа адресата; выше — `forbidden` с пояснением потолка.
- `VacationResponse` (RFC 8621 §8): внешние тема и текст (CTR-93) — свойства `externalSubject`, `externalTextBody`,
  `externalHtmlBody` под возможностью `urn:thepost:vacation`; без неё клиент видит стандартный объект, внешний текст не
  меняется. Пустой внешний текст — чужим ответа нет.

## Как делают другие
- Внешний автоответ — отдельный текст: Exchange `ExternalMessage`
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-mailboxautoreplyconfiguration).
- Каталог только своего арендатора: адресные политики Exchange
  (https://learn.microsoft.com/en-us/exchange/address-books/address-book-policies/turn-on-address-book-policy-routing).
- Уровни внешнего обмена: Google (https://knowledge.workspace.google.com/admin/calendar/set-google-calendar-sharing-options),
  Exchange Sharing Policy (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-sharingpolicy).
- Отличие: внешний текст — расширение под своей возможностью; стандартный JMAP одного текста не нарушается.

## Приёмка
- Тесты: `Principal/query` не находит принципала второй организации узла; `shareWith` с ним — `invalidProperties`;
  `VacationResponse/set` с `externalTextBody` под `urn:thepost:vacation` сохраняется, без возможности — свойство
  неизвестно (`invalidProperties`).
