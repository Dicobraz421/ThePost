# План волн The Post

Источник — SRS ред. 4 (`docs/spec/APP-G.md`, этапы Ф0–Ф4; базовая поставка и модули — `S1.md#1.3`, решение 2026-10-30). Карточки — `docs/tasks/<ID>.md`.
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
| ✅ contracts-01 | CTR-01, CTR-02, CTR-03, CTR-04 | B | — | 1 |
| ✅ sys-01 | SYS-01, SYS-02, SYS-03, SYS-04 | A/B | — | 2 |
| ✅ deploy-01 | DEP-01, DEP-02, DEP-03 | A | — | 3 |
| ✅ ci-01 | CI-01 | B | — | 4 |
| ✅ contracts-02 | CTR-05, CTR-10 | B | contracts-01 | 1 |
| ✅ testkit-01 | KIT-01, KIT-02, KIT-03, KIT-04 | A | contracts-01 | 1 |
| ✅ platform-01 | PLT-01, PLT-02, PLT-03, PLT-04 | A | contracts-01, SYS-01 | 2 |
| ✅ meta-01 | META-01, META-02, META-03, META-04 | B | contracts-01 | 3 |
| ✅ cluster-01 | CLU-01, CLU-02, CLU-03 | A | contracts-01 | 4 |
| ✅ blob-01 | BLOB-01, BLOB-02, BLOB-04 | A | contracts-01 | 4 |
| ✅ platform-02 | PLT-05, PLT-06, PLT-07, PLT-08 | A | platform-01, SYS-03 | 2 |
| ✅ meta-02 | META-05, META-06, META-07, META-08, META-13 | B | meta-01, contracts-02 | 3 |
| ✅ blob-02 | BLOB-03, BLOB-05, BLOB-06, BLOB-07, BLOB-08 | A | blob-01, testkit-01 | 4 |
| ✅ mailstore-01 | MS-01, MS-02, MS-03, MS-04 | A | contracts-02, testkit-01, blob-02 | 1 |
| ✅ app-01 | APP-01, APP-02, APP-03 | A | platform-03, meta-02, mailstore-01, cluster-02 | 2 |

### Решения владельца 2026-10-30 (`docs/decisions/2026-10-30-owner-answers.md`)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-04 (ветка `x/contracts-04`) | CTR-11, CTR-12 | B | contracts-02 | 1 |
| ✅ sys-02 | SYS-05 | B | sys-01 | 2 |
| ✅ ci-02 | CI-02 | B | ci-01 | 3 |
| ✅ deploy-03 | DEP-06 | A | deploy-01 | 3 |
| ✅ cluster-02 | CLU-04, CLU-05 | A | cluster-01, contracts-04, ci-02 | 4 |
| ✅ platform-03 | PLT-09, PLT-10, PLT-11 | A | platform-02, cluster-02, contracts-04 | 2 |
| ✅ meta-04 | META-14 | A | meta-02, contracts-04 | 3 |
| ✅ blob-03 | BLOB-09, BLOB-10 | A | blob-02, contracts-04 | 4 |

### Решения владельца 2026-10-30, секреты и журнал (`docs/decisions/2026-10-30-config-events-secrets.md`)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-05 | CTR-13, CTR-14, CTR-15 | B | contracts-04 | 1 |
| ✅ deploy-04 | DEP-07 | A | deploy-03 | 3 |
| ✅ testkit-03 | KIT-06 | A | contracts-05 | 1 |
| ✅ meta-05 | META-15, META-16, META-17 | A/B | meta-04, contracts-05 | 3 |
| ✅ cluster-03 | CLU-06 | A | cluster-02, contracts-05 | 4 |
| ✅ platform-04 | PLT-12, PLT-13 | B | platform-03, contracts-05 | 2 |
| ✅ meta-06 | META-18, META-19 | A | meta-05, platform-04 | 3 |

### Остаток связки Ф0 (отчёт app-01, PR #89; конверт ingest — `docs/decisions/2026-10-31-ingest-envelope.md`)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ blob-04 | BLOB-11 | A | blob-03, contracts-05 | 4 |
| ✅ cluster-04 | CLU-07 | A | cluster-03 | 4 |
| ✅ platform-05 | PLT-14 | A | platform-04 | 2 |
| ✅ contracts-06 (ветка `x/contracts-06`) | CTR-16 | B | contracts-02 | 1 |
| ✅ mailstore-03 | MS-08 | A | mailstore-01, contracts-06 | 1 |
| ✅ app-03 | APP-07 | A | app-01, platform-04, blob-04, cluster-04, meta-06 | 2 |
| ✅ blob-05 | BLOB-12 | A | app-03 | 4 |
| ✅ cluster-05 | CLU-08 | A | app-03 | 4 |

Выход Ф0: `thepost serve` с минимальным config и `roles: [all]` доходит до ready; RLS по tenant включён; журнал событий и точки расширения работают; `doctor` и `verify` работают; протокол BLOB проверен тестом гонки.

## Ф1. MVP SMB (≈6 мес.)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-03 | CTR-06, CTR-07, CTR-08, CTR-09 | B | Ф0 | 1 |
| ✅ testkit-02 | KIT-05 | A | contracts-03 | 1 |
| ✅ meta-03 | META-09, META-10, META-11, META-12 | B | contracts-03 | 3 |
| ✅ mailstore-02 | MS-05, MS-06, MS-07 | A | contracts-03 | 4 |
| ✅ contracts-07 | CTR-18 | B | contracts-03 | 1 |
| ✅ mailstore-04 | MS-09 | A | mailstore-02, testkit-02 | 4 |
| contracts-08 (ветка `x/contracts-08`) | CTR-19 | B | meta-03 | 1 |
| ✅ contracts-09 (ветка `x/contracts-09`) | CTR-20 | B | contracts-06 | 1 |
| ✅ mailstore-05 | MS-10 | A | contracts-09 | 4 |
| mailstore-06 | MS-11 | A | mailstore-02 | 4 |
| ✅ meta-07 | META-20 | A | — | 3 |
| ✅ auth-01 | AUTH-01, AUTH-02, AUTH-03, AUTH-04 | A | contracts-03 | 1 |
| ✅ search-01 | SRCH-01, SRCH-02, SRCH-03, SRCH-04 | A | contracts-03 | 3 |
| ✅ smtp-01 | SMTP-01, SMTP-02, SMTP-03, SMTP-04 | A | auth-01, testkit-02 | 4 |
| auth-02 | AUTH-05, AUTH-06 | A | auth-01, contracts-08 | 1 |
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

### Решение 2026-11-01: заимствования и базовые принципы безопасности

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-10 (ветка `x/contracts-10`) | CTR-21 | B | contracts-09 | 1 |
| sys-03 | SYS-06, SYS-07 | B | sys-01 | 2 |
| ci-03 | CI-03, CI-04 | B | ci-01 | 4 |
| platform-06 | PLT-15, PLT-16 | A | sys-03, contracts-10 | 2 |
| meta-08 | META-21, META-22, META-23, META-24 | B | contracts-10 | 3 |
| blob-06 | BLOB-13 | A | meta-08 | 4 |
| mailstore-07 | MS-12, MS-13 | A | mailstore-06 | 1 |
| smtp-04 | SMTP-12, SMTP-13, SMTP-14, SMTP-15 | A | smtp-03, contracts-12 | 4 |
| smtp-05 | SMTP-16, SMTP-17, SMTP-18, SMTP-19 | A | smtp-04, meta-08, mailstore-07 | 4 |
| auth-03 | AUTH-07 | A | auth-02, meta-08 | 1 |
| admin-02 | ADM-05, ADM-06, ADM-07 | A | admin-01, meta-08 | 3 |
| imap-04 | IMAP-13 | A | imap-03 | 2 |
| jmap-03 | JMAP-09 | A | jmap-02 | 3 |
| deploy-05 | DEP-08 | A | deploy-02, meta-08, platform-06 | 4 |

### Решение 2026-11-01: взаимодействие компонентов и требования к СУБД

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-11 (ветка `x/contracts-11`) | CTR-22 | B | contracts-10 | 1 |
| meta-09 | META-25 | A | meta-08 | 3 |
| meta-10 | META-26, META-27 | A | meta-09, contracts-11 | 3 |
| cluster-06 | CLU-09, CLU-10, CLU-11 | A | contracts-11 | 4 |
| app-04 | APP-08 | A | meta-10, cluster-06 | 2 |
| platform-07 | PLT-17 | A | platform-06 | 2 |
| mailstore-08 | MS-14 | A | mailstore-07 | 1 |
| blob-07 | BLOB-14 | A | blob-06 | 4 |
| auth-04 | AUTH-08 | A | auth-03, meta-10 | 1 |
| ci-04 | CI-05 | B | ci-03, cluster-06 | 4 |
| deploy-06 | DEP-09 | A | deploy-05, cluster-06 | 4 |

### Решение 2026-11-01: состав функций транспорта и внешний MTA

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-12 (ветка `x/contracts-12`) | CTR-23 | B | contracts-11 | 1 |
| meta-11 | META-28 | A | meta-10, contracts-12 | 3 |
| smtp-06 | SMTP-20, SMTP-21, SMTP-22, SMTP-23 | A | smtp-05, contracts-12 | 4 |
| admin-03 | ADM-08 | A | admin-02, contracts-12 | 3 |
| jmap-04 | JMAP-10 | A | jmap-03, contracts-12 | 3 |
| app-05 | APP-09 | A | meta-11, smtp-06, admin-03, jmap-04 | 2 |

Выход Ф1: MVP SMB — критерии `docs/spec/APP-D.md#Д.7` для профиля SMB на стенде DEP-04.

## Ф2–Ф4 базовой поставки (карточки — по завершении Ф1)

| Этап | Полосы | Содержание |
|---|---|---|
| Ф2. Совместная работа (≈5 мес.) | dav, jmap, sieve, auth | CalDAV, CardDAV, GAL, scheduling, iTIP/iMIP, free/busy, ресурсы; JMAP Contacts/Calendars/Sieve; Sieve и ManageSieve |
| Ф3. Enterprise (≈5 мес.) | smtp, blob, cluster, admin, app, deploy | Профиль S3, межузловые запросы для ролей search и admin (SRS 2.8), режим edge (без очереди, сквозная передача на transport, реплика получателей, SMTP mTLS, секреты edge — из хранилища по отдельной политике — SRS 2.6, решение 2026-11-01), пресеты transport/access/backend/mailbox, экспорт/импорт, эталонные Ansible-роли и Helm-чарт (SRS 5.6), стенд Enterprise и испытания Д.6 |
| Ф4. Выпуск (≈3 мес.) | все | Стабилизация, аудит безопасности, фаззинг парсеров, документация администратора |

## Дополнительные модули (SRS 1.3; после Ф1, порядок — решение владельца)

Каждый модуль — cargo feature + `modules.<имя>.enabled`, только через точки расширения (SRS 2.5), данные в схеме `mod_<имя>`, полоса `mod-<имя>`
(добавляется в `lanes.toml` PR класса B при старте модуля). Модули: Мультитенантность, Соответствие требованиям (политики хранения, Legal Hold,
журналирование, eDiscovery), Шардирование, Миграция бэкенда, Межсайтовый DR, Развёртывание (GUI), Веб-клиент, Миграция с внешних систем,
Генератор нагрузки, ГОСТ-TLS.
