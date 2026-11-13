# CMP-10: приёмка Ф2 — календари, контакты, правила, автоответ — ручная проверка

Критерий Ф2: календари, контакты, правила и автоответы проверены клиентами. Здесь — сводный сценарий беты на уровне
протоколов и клиентов. Подробные шаги клиентов — в `DAV-08.md` (календари), `DAV-11.md` (контакты), `APP-28.md`
(встречи), `APP-29.md` (подключение), `APP-26.md` (правила и автоответ), `SIEVE-09.md` (ManageSieve),
`JMAP-28.md` (JMAP в Postman). Консоль администратора и нагрузка k6 — отдельные проверки карточки CMP-10.

Нужны: бета с ролью `all` за TLS (эталон `--profile smb`, `docs/manual-tests/CMP-01.md`), публичное имя
(`mail.example.org`) с сертификатом, которому доверяют клиенты; администратор и сессия `S=tps_…`
(`thepost admin init`, `thepost admin login`); каталог без LDAP (секции `auth.ldap` нет) — учётные записи и группы
заведены через Admin API. Учётные записи с паролями приложений (`APP-19.md`):

| Обозначение | Назначение |
|---|---|
| `org@`, `att@` | организатор и участник встреч, CalDAV |
| `m1@`, `m2@` | участники группы `team@` |
| `m3@` | участник группы `all@` напрямую |
| `card@` | контакты, книга «Организация» |
| `j1@`, `j2@` | JMAP: календари, контакты, автоответ, правила |
| `hid@` | учётная запись, скрытая из книги «Организация» |

Обозначения: `B=https://mail.example.org`, `U=<адрес>:<пароль приложения>`, `ADM=https://127.0.0.1:8443/api/v1/orgs/$ORG`,
`H="Authorization: Bearer $S"`, `J='Content-Type: application/json'`. Запросы — `curl` (или любой HTTP-клиент),
ManageSieve — `openssl s_client -starttls sieve`, письма — `swaks`.

Колонка «Клиент» в каждом разделе: **протокол** — шаг проверяется запросами без клиента; **клиент** — нужен
живой клиент (Thunderbird 128+, Календарь и Контакты iOS 17+ и macOS 14+, DAVx5 4.4+, Evolution 3.50+, Postman,
дополнение Sieve для Thunderbird), протокольная часть шага — предварительная проверка.

Результат шага — «ок», «не прошёл» (что делал, что ожидал, что получил) или «нужен клиент».

## Подготовка каталога (Admin API)
1. Группа `team@`: `curl -X POST -H "$H" -H "$J" $ADM/groups -d '{"name":"Команда","address":"team@example.org",
   "members":[{"account":"<id m1>"},{"account":"<id m2>"}]}'` — `201`.
2. Вложенная группа `all@`: участники — `{"group":"<id team>"}` и `{"account":"<id m3>"}` — `201`.
3. Скрытая группа `hidden-grp@` (`"hidden_from_gal": true`) из `m1` — `201`.
4. Почтовые контакты: `POST $ADM/contacts` — `{"display_name":"Партнёр","email":"p@partner.example"}` и
   `{"display_name":"Скрытый партнёр","email":"h@partner.example","hidden_from_gal":true}` — `201`.
5. Псевдоним учётной записи `att@` — `att-alias@example.org`; признак «скрыт из книги» у `hid@`. Источник — каталог
   (`auth.ldap`) или Admin API; если узел не даёт задать их без LDAP — шаги 2.6 и 3.4 «не прошёл» с причиной.
6. Подождать `maintenance.gal_interval` (по умолчанию 1 мин): книга «Организация» перестраивается из каталога.

## 1. Подключение и автонастройка
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 1.1 | `GET $B/.well-known/caldav`, `/.well-known/carddav` | `301`, `Location` — `/dav/` | протокол |
| 1.2 | `PROPFIND -H 'Depth: 1' $B/dav/calendars/org@…/`; `PROPFIND -H 'Depth: 0' $B/dav/principals/org@…/` с `schedule-inbox-URL`, `schedule-outbox-URL` | дом: `207`, `calendar/`, `tasks/` (без `inbox/` и `outbox/`); принципал: `…/inbox/` и `…/outbox/`, их прямой `PROPFIND` — `207` (RFC 6638) | протокол |
| 1.3 | `OPTIONS $B/dav/calendars/org@…/` | `DAV` содержит `calendar-access`, `calendar-auto-schedule` | протокол |
| 1.4 | `PROPFIND -H 'Depth: 1' $B/dav/addressbooks/card@…/` | `207`: `contacts/` и `gal/` «Организация» | протокол |
| 1.5 | `GET $B/.well-known/jmap` | capability `calendars`, `contacts`, `vacationresponse`, `sieve` | протокол |
| 1.6 | Подключение Thunderbird, iOS, macOS, DAVx5, Evolution по одному адресу сервера (`APP-29.md`, «Клиенты») | найдены календарь, задачи, контакты, «Организация» | клиент |

## 2. Календари CalDAV
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 2.1 | `PUT calendar/<uid>.ics` событие с кириллицей; `GET`; `PUT` с `If-Match` старого ETag | `201`, тот же текст; `412` на устаревший ETag | протокол |
| 2.2 | Повтор: еженедельно ×5, третье вхождение перенесено (`RECURRENCE-ID`), четвёртое исключено (`EXDATE`); `REPORT calendar-query` с `expand` на 6 недель | 4 вхождения, перенесённое — на новом месте | протокол |
| 2.3 | `REPORT sync-collection` до и после правки | новый токен, в изменениях — правленый объект; удалённый — `404` | протокол |
| 2.4 | Недопустимые данные: два разных `UID`; событие 1850 года | `403 valid-calendar-object-resource`; `403 min-date-time` | протокол |
| 2.5 | Сценарии DAV-08 1–10 (создание, весь день, правка, повтор, офлайн, два устройства, новый календарь) | `DAV-08.md` | клиент |

### Задачи VTODO
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 2.6 | `PUT tasks/<uid>.ics` VTODO со сроком `DUE`; правка `STATUS:COMPLETED`, `COMPLETED`, `PERCENT-COMPLETE:100` | `201`, `204`; `GET` — завершённая задача | протокол |
| 2.7 | VTODO в `calendar/`, VEVENT в `tasks/` | `403 supported-calendar-component` | протокол |
| 2.8 | `REPORT calendar-query` `comp-filter VTODO` с `is-not-defined COMPLETED` | только незавершённые | протокол |
| 2.9 | Задача со сроком в Thunderbird, отметка «выполнено» в «Напоминаниях» iOS/macOS, jtx Board, Evolution (DAV-08 п. 9) | состояние совпадает | клиент |

### Встречи (CalDAV Scheduling, RFC 6638)
Организатор — `org@`. После каждого шага проверяется объект организатора (`SCHEDULE-STATUS` участника) и календарь
участника (`PROPFIND`/`REPORT calendar-query` в `calendar/` участника, `inbox/` — сообщения iTIP).

| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 2.10 | `org@` создаёт встречу с `ATTENDEE:mailto:att@` | у участника `SCHEDULE-STATUS=1.2`; у `att@` событие в `calendar/` с `PARTSTAT=NEEDS-ACTION` | протокол |
| 2.11 | `att@` меняет свой `PARTSTAT` на `ACCEPTED` (`PUT` своей копии) | у организатора `PARTSTAT=ACCEPTED` без его действий | протокол |
| 2.12 | `org@` переносит встречу на час | у `att@` новое время, `PARTSTAT` сброшен в `NEEDS-ACTION` | протокол |
| 2.13 | `org@` удаляет встречу | у `att@` событие `STATUS:CANCELLED` или удалено | протокол |
| 2.14 | **Псевдоним:** участник `mailto:att-alias@` | приглашение получает `att@`; его ответ доходит организатору (`SCHEDULE-STATUS=1.2`) | протокол |
| 2.15 | **Группа:** участник `mailto:team@` | в объекте организатора `team@` с `CUTYPE=GROUP` и участники `m1@`, `m2@` с `MEMBER="mailto:team@"`; у `m1@`, `m2@` событие; самой группе сообщение не отправлено | протокол |
| 2.16 | **Вложенная группа:** участник `mailto:all@` | раскрыты `m1@`, `m2@` (через `team@`) и `m3@`, каждый по одному разу; у всех троих событие | протокол |
| 2.17 | Скрытая из книги группа `hidden-grp@` в участниках | раскрывается так же: скрытие касается книги, а не планирования | протокол |
| 2.18 | Встреча Thunderbird ↔ iOS, ответы, перенос, отмена, повторяющаяся (APP-28 п. 3–8), приглашение группы из клиента | `APP-28.md` | клиент |
| 2.19 | Внешний участник (Gmail, Outlook.com) — iMIP наружу и ответы извне (APP-28 п. 9–13) | `APP-28.md` | клиент (нужен внешний ящик и DNS) |

### Занятость
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 2.20 | У `att@` два события: обычное и `TRANSP:TRANSPARENT`; `org@` — `POST outbox/` с `VFREEBUSY` `METHOD:REQUEST` по `att@` и внешнему адресу | `schedule-response`: у `att@` `2.0` и `FREEBUSY` только обычного события; внешнему — `3.7` | протокол |
| 2.21 | `att@` — `REPORT free-busy-query` на свой `calendar/` | `VFREEBUSY` с тем же периодом | протокол |
| 2.22 | «Проверить доступность» в Календаре macOS, «Расписание» Evolution (APP-28 п. 14, DAV-08 п. 11) | занято только время непрозрачного события | клиент |

## 3. Контакты и книга «Организация» без LDAP
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 3.1 | CRUD в `contacts/` (vCard 3.0 и 4.0), `If-Match`, версия по `Accept`, `addressbook-query` кириллицей | как в DAV-11 п. 5, 6, 11 | протокол |
| 3.2 | `PROPFIND -H 'Depth: 1' gal/`, `GET` карточек | карточки всех видимых учётных записей организации, групп с адресом (`KIND:group`) и почтовых контактов | протокол |
| 3.3 | `PUT`, `DELETE`, `PROPPATCH` в `gal/` | `403 need-privileges` | протокол |
| 3.4 | **Скрытие:** учётная запись `hid@`, группа `hidden-grp@`, контакт «Скрытый партнёр» | в `gal/` их нет; после снятия признака — появляются не позже `gal_interval` | протокол |
| 3.5 | `REPORT sync-collection` на `gal/` до и после изменения каталога (новая группа или контакт) | новый токен, в изменениях — новая карточка | протокол |
| 3.6 | Сценарии DAV-11 1–10 в каждом клиенте; «Организация» видна в Thunderbird, iOS, macOS, DAVx5, Evolution, поиск по ней; адрес группы `team@` подставляется в «Кому» | `DAV-11.md` | клиент |

## 4. JMAP
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 4.1 | `Calendar/get`, `Calendar/set` (создать «Проверка JMAP»), `Calendar/changes` | календарь виден и по CalDAV (`PROPFIND`) | протокол |
| 4.2 | `CalendarEvent/set` create, `CalendarEvent/query` по интервалу, `CalendarEvent/get`, `/changes` | событие читается и по CalDAV, правка по CalDAV видна в JMAP | протокол |
| 4.3 | `CalendarEvent/set` с участником `j2@` и `sendSchedulingMessages: true` | у `j2@` событие в календаре | протокол |
| 4.4 | `CalendarEventNotification/get`, `/query`, `/changes` у `j2@` после 4.3; `CalendarEventNotification/set` destroy | уведомление о новом событии с автором `j1@`; после destroy `/changes` сообщает удаление | протокол |
| 4.5 | `ParticipantIdentity/get` у `j1@` | адрес `j1@` | протокол |
| 4.6 | `AddressBook/get`: книга по умолчанию и «Организация» (`myRights` без записи); `ContactCard/query` в «Организации» | `hid@` не найдена, `j2@` найдена; `ContactCard/set` в «Организацию» — `forbidden` | протокол |
| 4.7 | `ContactCard/set` в свою книгу | карточка видна по CardDAV | протокол |
| 4.8 | `VacationResponse/set` (`isEnabled`, тема, текст) | `GET …/accounts/<id>/vacation` Admin API — те же значения; письмо `j2@` → `j1@` — ровно один автоответ за период | протокол |
| 4.9 | Сценарии JMAP-28 и APP-26 «Автоответ в Postman» в Postman (push без перезапуска) | `JMAP-28.md`, `APP-26.md` | клиент |

Примечание к 4.4: лента работает без настройки с dav-14 (DAV-36): `dav.schedule_keep_processed` по умолчанию `true`,
применённые сообщения iTIP хранятся `dav.schedule_processed_ttl` (30 суток) и удаляются ролью maintenance. На
сборке до dav-14 нужно `dav.schedule_keep_processed: true` в конфигурации, иначе лента пуста.

## 5. Правила Sieve
| № | Шаг | Ожидается | Клиент |
|---|---|---|---|
| 5.1 | ManageSieve 4190: `AUTHENTICATE` до STARTTLS | отказ `ENCRYPT-NEEDED` | протокол |
| 5.2 | После STARTTLS: `CAPABILITY`, `CHECKSCRIPT` с ошибкой, `PUTSCRIPT`, `SETACTIVE`, `LISTSCRIPTS`, `GETSCRIPT`, `DELETESCRIPT` активного | `NO` со строкой и столбцом; `"<имя>" ACTIVE`; `NO (ACTIVE)` | протокол |
| 5.3 | Письмо с меткой в теме — `fileinto :create` в папку правила; без метки — во «Входящие» (IMAP) | как ожидается | протокол |
| 5.4 | JMAP Sieve: `SieveScript/validate` неверного, `SieveScript/set` (blob) с `onSuccessActivateScript`, `SieveScript/query` | `invalidSieve`; скрипт активен; тот же скрипт виден в ManageSieve `LISTSCRIPTS` | протокол |
| 5.5 | Письмо по правилу JMAP | в папке правила | протокол |
| 5.6 | Дополнение Sieve для Thunderbird (SIEVE-09, «Thunderbird»): создание, ошибка с позицией, активация, переименование | `SIEVE-09.md` | клиент |

## Итог
| Раздел | Протокол | Клиенты |
|---|---|---|
| 1. Подключение | | |
| 2. Календари, задачи, встречи, занятость | | |
| 3. Контакты, «Организация» | | |
| 4. JMAP | | |
| 5. Правила | | |

Проверка пройдена, когда нет «не прошёл»; найденное — карточки `FIX-<NN>` со ссылкой на номер шага.
