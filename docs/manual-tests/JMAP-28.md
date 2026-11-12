# JMAP-28: календарь и контакты по JMAP — ручная проверка клиентом Postman и Thunderbird

Нужны: узел с ролями `jmap` и `dav` и подключённым хранилищем календарей и контактов (в объекте сессии JMAP есть
`urn:ietf:params:jmap:calendars` и `urn:ietf:params:jmap:contacts`), учётная запись `user@example.org` с паролем
приложения. Клиенты: Postman (почтовый клиент проекта Pets; JMAP Calendars и Contacts) и Thunderbird 128+
(CalDAV и CardDAV, адрес `https://mail.example.org/dav/` или автонастройка по `/.well-known/caldav`).

Сервер реализует JMAP Calendars по текущему черновику IETF с JSCalendar 2.0 (`recurrenceRule`, `calendarAddress`);
отличия перечислены в комментариях модулей `calendars` и `calendar_event` крейта `tp-jmap`.

## Подготовка
1. `thepost serve`; в Postman добавить учётную запись по адресу JMAP, в Thunderbird — календарь и адресную книгу
   по CalDAV и CardDAV той же учётной записи.
2. Проверка без клиентов: `curl -su user@example.org:<пароль приложения> https://mail.example.org/.well-known/jmap`
   → в `capabilities` обе capability, в `accountCapabilities` календарей — `maxCalendarsPerEvent: 1`.

## Сценарии
1. Календарь. В Postman создать календарь «Работа» с цветом → в Thunderbird он появляется в списке календарей
   учётной записи с тем же именем и цветом. Переименовать в Thunderbird → имя меняется в Postman без перезапуска
   (push `Calendar`).
2. Простое событие. В Postman создать событие «Планёрка» на завтра 10:00–11:00 (пояс Europe/Moscow) с местом и
   описанием → в Thunderbird то же время, место, описание. Изменить время в Thunderbird → в Postman новое время.
3. Весь день. В Thunderbird создать событие «Выходной» на весь день → в Postman оно без времени, в тот же день
   (не сдвигается на соседний день). Создать такое же в Postman → в Thunderbird — событие на весь день.
4. Повтор с исключением. В Postman создать еженедельное событие на 4 недели; одно вхождение перенести на другой
   день, одно удалить → в Thunderbird три вхождения: два по расписанию и перенесённое. В Thunderbird изменить
   название одного вхождения («только это событие») → в Postman изменилось только оно.
5. Участники. В Thunderbird создать событие с участником `petr@example.org` → в Postman виден участник с именем,
   адресом и статусом «ожидает ответа». Добавить второго участника в Postman → он виден в Thunderbird, первый
   не потерян. Приглашения при этом не отправляются (отправка — JMAP-31).
6. Оповещение. В Postman добавить напоминание за 10 минут → в Thunderbird у события то же напоминание; изменить
   в Thunderbird на 30 минут → в Postman 30 минут.
7. Удаление. Удалить событие в Postman → исчезает в Thunderbird после синхронизации; удалить в Thunderbird →
   исчезает в Postman (push `CalendarEvent`).
8. Контакты. В Postman создать контакт с именем, двумя адресами почты и телефоном → в Thunderbird те же поля.
   Изменить телефон в Thunderbird → в Postman новый телефон, остальные поля на месте. Удалить в Postman →
   исчезает в Thunderbird.
9. Сохранность данных. Событие, созданное Thunderbird (со своими `X-MOZ-*` свойствами и `VTIMEZONE`),
   переименовать в Postman → скачать `.ics` (`curl` по адресу ресурса CalDAV): свойства `VCALENDAR`
   (`PRODID`) и нестандартные свойства события на месте, изменилось только `SUMMARY` и `LAST-MODIFIED`/`DTSTAMP`.

## Итог
Записать версии Postman и Thunderbird, результат п. 1–9; расхождения — с текстом `.ics` до и после
(`curl` по адресу ресурса) и ответом `CalendarEvent/get`.
