# План волн The Post

Источник — SRS ред. 3 (`docs/spec/APP-G.md`, этапы Ф0–Ф4). Карточки — `docs/tasks/<ID>.md`.
Волна = 3–5 карточек одной полосы = ветка `w/<полоса>-<NN>` = один PR. Порядок внутри волны — по `deps`.
Волна стартует, когда закрыты все `deps` её карточек из других полос (столбец «Ждёт»).

## Процесс
- Параллельно — до 4 агентов (все полосы собираются на Linux → облачные агенты, `docs/DEV.md#Агенты`).
- Класс B (контракты, миграции, sandbox, CI, зависимости) — approve владельца; остальное — автомерж на `gate`.
- Оркестратор после мержа волны: отмечает волну здесь (✅) и запускает следующие готовые.
- Бета — ночной тег `beta-*` (CI-01); найденное → карточки `FIX-<NN>`.

## Ф0. Основа (≈3 мес.)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-01 | CTR-01, CTR-02, CTR-03, CTR-04, CTR-05 | B | — | 1 |
| sys-01 | SYS-01, SYS-02, SYS-03, SYS-04 | A/B | — | 2 |
| deploy-01 | DEP-01, DEP-02, DEP-03 | A | — | 3 |
| ci-01 | CI-01 | B | — | 4 |
| testkit-01 | KIT-01, KIT-02, KIT-03, KIT-04 | A | contracts-01 | 1 |
| platform-01 | PLT-01, PLT-02, PLT-03, PLT-04 | A | contracts-01, SYS-01 | 2 |
| meta-01 | META-01, META-02, META-03, META-04 | B | contracts-01 | 3 |
| cluster-01 | CLU-01, CLU-02, CLU-03 | A | contracts-01 | 4 |
| blob-01 | BLOB-01, BLOB-02, BLOB-04 | A | contracts-01 | 4 |
| platform-02 | PLT-05, PLT-06, PLT-07, PLT-08 | A | platform-01, SYS-03 | 2 |
| meta-02 | META-05, META-06, META-07, META-08 | A | meta-01 | 3 |
| blob-02 | BLOB-03, BLOB-05, BLOB-06, BLOB-07, BLOB-08 | A | blob-01, testkit-01 | 4 |
| mailstore-01 | MS-01, MS-02, MS-03, MS-04 | A | testkit-01, blob-02 | 1 |
| app-01 | APP-01, APP-02, APP-03 | A | platform-02, meta-02, mailstore-01, cluster-01 | 2 |

Выход Ф0: `thepost serve` с минимальным config доходит до ready; `doctor` и `verify` работают; протокол BLOB проверен тестом гонки.

## Ф1. MVP SMB (≈6 мес.)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-02 | CTR-06, CTR-07, CTR-08, CTR-09 | B | Ф0 | 1 |
| testkit-02 | KIT-05 | A | contracts-02 | 1 |
| meta-03 | META-09, META-10, META-11, META-12 | B | contracts-02 | 3 |
| mailstore-02 | MS-05, MS-06, MS-07 | A | contracts-02 | 4 |
| auth-01 | AUTH-01, AUTH-02, AUTH-03, AUTH-04 | A | contracts-02 | 1 |
| search-01 | SRCH-01, SRCH-02, SRCH-03, SRCH-04 | A | contracts-02 | 3 |
| smtp-01 | SMTP-01, SMTP-02, SMTP-03, SMTP-04 | A | auth-01, testkit-02 | 4 |
| auth-02 | AUTH-05, AUTH-06 | A | auth-01 | 1 |
| jmap-01 | JMAP-01, JMAP-02, JMAP-03, JMAP-04 | A | auth-01, mailstore-02, testkit-02 | 3 |
| imap-01 | IMAP-01, IMAP-02, IMAP-03, IMAP-04 | A | testkit-02, auth-01, mailstore-02 | 2 |
| smtp-02 | SMTP-05, SMTP-06, SMTP-07, SMTP-08 | A | smtp-01, meta-03 | 4 |
| imap-02 | IMAP-05, IMAP-06, IMAP-07, IMAP-08 | A | imap-01 | 2 |
| search-02 | SRCH-05, SRCH-06, SRCH-07, SRCH-08 | A | search-01, meta-03 | 1 |
| admin-01 | ADM-01, ADM-02, ADM-03, ADM-04 | A | auth-01, mailstore-02 | 3 |
| smtp-03 | SMTP-09, SMTP-10, SMTP-11 | A | smtp-02 | 4 |
| jmap-02 | JMAP-05, JMAP-06, JMAP-07, JMAP-08 | A | jmap-01, smtp-02 | 3 |
| imap-03 | IMAP-09, IMAP-10, IMAP-11, IMAP-12 | A | imap-02, auth-02 | 2 |
| app-02 | APP-04, APP-05, APP-06 | A | smtp-01, imap-01, jmap-01, search-01, admin-01 | 1 |
| deploy-02 | DEP-04, DEP-05 | A | app-02 | 4 |

Выход Ф1: MVP SMB — критерии `docs/spec/APP-D.md#Д.7` для профиля SMB на стенде DEP-04.

## Ф2–Ф4 (карточки — по завершении Ф1)

| Этап | Полосы | Содержание |
|---|---|---|
| Ф2. Совместная работа (≈5 мес.) | dav, jmap, sieve, auth | CalDAV, CardDAV, GAL, scheduling, iTIP/iMIP, free/busy, ресурсы; JMAP Contacts/Calendars/Sieve; Sieve и ManageSieve |
| Ф3. Enterprise (≈5 мес.) | blob, meta, cluster, admin, app, deploy | Профиль S3, Valkey, перенос ящиков между шардами, экспорт/импорт, Legal Hold, миграция бэкенда, Helm, Ansible Enterprise, стенд Enterprise и испытания Д.6 |
| Ф4. Выпуск (≈3 мес.) | все | Стабилизация, аудит безопасности, фаззинг парсеров, документация администратора |
