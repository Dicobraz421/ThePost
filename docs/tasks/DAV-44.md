# DAV-44: матрица доверия в CalDAV и CardDAV — iTIP извне, занятость, общий доступ, публикация, GAL

lane: dav
deps: [CTR-93, DAV-43]
class: A
files: [crates/tp-dav/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/research/2026-11-14-trust-matrix.md, docs/tasks/CTR-93.md, docs/tasks/DAV-43.md, docs/ADMIN-GUIDE.md#матрица-по-умолчанию]
checks: [nextest]
manual: no

## Сделать
- `ItipInbox::deliver` — по действующей матрице получателя (CTR-93) вместо `TrustMatrix::default()`:
  - новый `REQUEST` чужого: `Button` — ничего не вносится (письмо уже в ящике), `Auto` или итог `processcalendar`
    в пределах потолка — вносится как от своего, но без бронирования;
  - `REPLY` и `CANCEL` чужого (`ApplyIfExisting`) при `itip_updates = On` — только к событию с тем же UID в календаре
    получателя: `REPLY` — получатель — `ORGANIZER`, отправитель — `ATTENDEE` события, меняется только его `PARTSTAT`
    (и `DELEGATED-*`), прочие свойства из письма не берутся (RFC 5546 §3.2.3, §6.1.1); `CANCEL` — отправитель —
    `ORGANIZER` события, получатель — `ATTENDEE` (RFC 5546 §6.1.2, RFC 6638 §11.2); иначе — только письмо;
  - `REFRESH`, `COUNTER`, `DECLINECOUNTER`, `ADD`, `PUBLISH` чужих — только письмо (RFC 5546 §6.1.6); бронирование
    ресурсов чужим — нет при любой политике (DAV-43). Свойства, способные навредить клиенту (внешние `ATTACH`,
    `VALARM` со звуком по ссылке), из письма чужого не вносятся (CVE-2023-23397).
- Занятость: запрос `VFREEBUSY` к `schedule-outbox` (RFC 6638 §5) — только принципалы своей организации; адрес
  соседней организации — `request-status` `3.7` без данных; уровень ответа своим — по клетке `freebusy` для `Own`.
- Общий доступ (`share.rs`): получатель доступа — только учётная запись своей организации; адрес соседа или внешний —
  403 с пояснением; уровень — не выше клетки `sharing` для `Own`.
- Публикация ICS и ссылки занятости FBURL (DAV-20…22, когда появятся): уровень ссылки — не выше клетки `sharing`
  (`External`; ссылка для домена-партнёра — `Partner`), занятость партнёру — только учётным записям группы, где
  `freebusy.partner` открыта; по умолчанию публикация выключена.
- GAL (книга «Организация»): только своя организация — тест на двух организациях одного узла.

## Как делают другие
- Ресурсы не принимают внешние приглашения: `ProcessExternalMeetingMessages=$false`
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-calendarprocessing); Google — только явно
  добавленные адреса (https://knowledge.workspace.google.com/admin/calendar/allow-non-google-users-to-book-resources).
- Приглашения извне — «только известные отправители», потолок администратора
  (https://workspaceupdates.googleblog.com/2022/07/invitations-from-known-senders-only-google-calendar.html,
  https://workspaceupdates.googleblog.com/2026/08/new-admin-controls-for-adding-invitations-to-Google-Calendar.html);
  Stalwart — `autoAddInvitations=false` (https://www.stalw.art/docs/ref/object/calendar-scheduling/).
- Занятость партнёру — уровни `AvailabilityOnly`/`LimitedDetails`, область — группа
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-organizationrelationship).
- GAL соседей не видна: адресные политики Exchange
  (https://learn.microsoft.com/en-us/exchange/address-books/address-book-policies/address-book-policies).
- Вредные свойства календаря — https://msrc.microsoft.com/blog/2023/03/microsoft-mitigates-outlook-elevation-of-privilege-vulnerability.
- Отличие: `REPLY` извне меняет только ответ участника — подмена времени или места через `REPLY` невозможна.

## Приёмка
- Тесты: `REQUEST` внешнего — события нет; партнёра при `Auto` — есть, при `Button` — нет; `REPLY` внешнего участника к
  событию нашего организатора — `PARTSTAT` обновлён, `SUMMARY` из письма не взят; `REPLY` от не-участника и к
  несуществующему UID — без изменений; `CANCEL` не от организатора — без изменений; `REFRESH` внешнего — ответа нет.
- `VFREEBUSY` на адрес соседней организации — `3.7`; общий доступ на внешний адрес — 403; GAL второй организации не
  виден первой.
