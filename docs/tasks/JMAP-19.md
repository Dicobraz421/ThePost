# JMAP-19: расширение Availability/get

lane: jmap
deps: [CTR-36, JMAP-14]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-calendar-external-api.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:thepost:availability`, метод `Availability/get`; источники по порядку: пользователи организации, FBURL (GAL, контакты, шаблон домена-партнёра), зарегистрированные `AvailabilitySource`.
- Внешние запросы: только HTTPS, запрет внутренних и частных адресов, таймаут и кэш из конфигурации; разбор VFREEBUSY; нет данных — `NoData`. Зависит от JMAP Calendars (Ф2).

## Формат (клиент Postman уже такой, S8-46)
Запрос `Availability/get`: `{ accountId, addresses: [String], utcStart, utcEnd }`; аккаунт — `primaryAccounts` расширения.
Ответ: `{ accountId, list: [{ address, source, detail, busyPeriods }] }`:
- `source`: `organization`, `link`, `partner`, `federation`; `detail`: `freeBusy` или `details`;
- `busyPeriods` — как у `Principal/getAvailability`: `[{ utcStart, utcEnd, busyStatus }]` (`confirmed`, `tentative`, `unavailable`);
- пустой список — свободен, `null` — `NoData`, «нет данных»; адрес — как в запросе, без учёта регистра.

## Приёмка
- Тесты: внутренний пользователь, FBURL с заглушкой HTTP, отказ для частного адреса, `NoData` вместо «свободен».
