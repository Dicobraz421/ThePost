# План работ The Post

Источник — SRS ред. 7 (`docs/spec/APP-G.md`, этапы Ф0–Ф4; базовая поставка и модули — `S1.md#1.3`, решение 2026-10-30). Карточки — `docs/tasks/<ID>.md`.
Волна = 3–5 карточек одной полосы = ветка `w/<полоса>-<NN>` = один PR. Порядок внутри волны — по `deps`.
Волна стартует, когда закрыты все `deps` её карточек из других полос (столбец «Ждёт»).

<!-- overview:begin -->
## Обзор этапов

Статус волн и карточек — `docs/tasks/README.md` (генерируется `scripts/plan_index.py`).

| Этап | Содержание | Волны | Карточки | Статус | Критерий завершения |
|---|---|---|---|---|---|
| Ф0. Основа | Платформа, хранилище метаданных и BLOB, CI, стенд SMB | 38/38 | 87/87 | ✅ готово | Узел SMB запускается из одного бинарного файла; метаданные и BLOB с тестами; CI ≤ 7 мин; стенд SMB поднимается |
| Ф1. MVP SMB | SMTP с очередью и Milter, IMAP, JMAP Mail, поиск, LDAP, Kerberos, OAUTHBEARER, корзина, Admin API, CLI | 282/327 | 391/448 | в работе, 87 % карточек | Почта SMB целиком на стенде: приём и отправка, IMAP и JMAP, поиск, вход; проверка клиентами Thunderbird и Postman; нагрузка Д.6 для SMB |
| Ф2. Совместная работа | CalDAV, CardDAV, JMAP Calendars и Contacts, scheduling, free/busy, Sieve, веб-консоль администратора | 94/162 | 157/234 | в работе, 67 % карточек | Календари, контакты, правила и автоответы проверены клиентами; консоль администратора покрывает Admin API |
| Ф3. Enterprise | S3, кластерные роли, межузловое взаимодействие, edge, шарды, Legal Hold, экспорт и импорт | 0/0 | 0/0 | карточки — после Ф1 | Схемы Enterprise-ВМ и Enterprise-K8s (приложение З) на стенде; переключение СУБД ≤ 30 с; нагрузка Д.6 для Enterprise |
| Ф4. Выпуск | Стабилизация, аудит безопасности, фаззинг парсеров, документация администратора | 1/2 | 1/2 | в работе, 50 % карточек | Аудит безопасности без открытых критичных находок; фаззинг парсеров; документация; выпуск 1.0 |

Открытые вопросы владельцу — issue с меткой «решение»; дополнительные модули — раздел в конце.
<!-- overview:end -->

## Процесс
- Параллельно — до 4 агентов (все полосы собираются на Linux → облачные агенты, `docs/DEV.md#Агенты`).
- Класс B (контракты, миграции, sandbox, CI, зависимости) — approve владельца; остальное — автомерж на `gate`.
- Оркестратор после мержа волны: отмечает волну здесь (✅), обновляет сводку `python3 scripts/plan_index.py`
  (обзор этапов выше и `docs/tasks/README.md` со статусами карточек) и запускает следующие готовые (статус ready)
  самого раннего этапа из раздела «Порядок работ».
- Новая карточка — сразу строкой волны в этом файле; карточки без волны сводка показывает в разделе «Вне волн».
- Карточка — файлы одной полосы; правка в двух полосах — две карточки и две волны (решение 71; межполосные по устройству — только класс B: контракт с `testkit`, `deps` с `Cargo.toml` крейта).
- Бета — ночной тег `beta-*` (CI-01); найденное → карточки `FIX-<NN>`.

## Порядок работ: сначала работающее изделие (решение 2026-11-11)

Принцип владельца: сначала базовые функции — чтобы как можно раньше получить работающее изделие; всё, без чего почта
работает, — дальше по плану (`docs/decisions/2026-11-11-working-product-first.md`). Оркестратор запускает готовые волны
только самого раннего незавершённого этапа; волны следующего этапа — когда в текущем нет готовых (свободная полоса); этапы 3 и 4 — после приёмки этапа 2, шаги 4.1–4.3 этапа 4 — раньше этапа 3 (решение 33).

**Срочно, первыми** (ошибки на живом узле, этап 2; реестр облачных волн, п. 26–27): `app-43` (FIX-03, домены организации в
Admin API — 500), `smtp-17` (FIX-01) → `app-44` (FIX-02, одобрение и отказ модерации — 500); паники от письма и ответа
фильтра (фаззер, п. 89) — `mailstore-13` (FIX-06, FIX-07), `smtp-22` (FIX-08); защита от потери данных при втором шарде —
`app-49` (FIX-05).

1. **Этап 1 — «почта работает на стенде SMB»** (срочно). Установка `--profile smb`, первый администратор, учётные записи,
   письмо между двумя пользователями, IMAP и JMAP, корзина, защита входа.
   - Контракты (в очереди): `contracts-29` (CTR-41), `contracts-17` (CTR-29), `contracts-21` (CTR-33).
   - `admin-06` (ADM-11) → `app-18` (APP-19, `thepost admin init`) → `meta-27` (META-43, токены в PostgreSQL) — первый администратор без OIDC (#193).
   - `meta-26` (META-42) и `testkit-05` (KIT-08) — системные папки по роли.
   - `app-06` (APP-10) — корзина, сжатие и нагрузка в узле; `app-16` (APP-17) — журнал доставки, группы, обучение антиспама, защита от перебора.
   - Хвосты PR 10–12.11: защита от перебора по адресу и подсети (SRS 6.3) — `contracts-39` (CTR-57) → `auth-09`, `smtp-15`,
     `imap-10`, `jmap-21` → `app-38`; срок журнала доставки и токенов `admin init` — `app-37`.
   - Приёмка этапа: `docs/manual-tests/CMP-01.md` целиком на бете (стенд DEP-04), клиенты Thunderbird и Postman.
2. **Этап 2 — «пилот SMB»**: удобство и надёжность эксплуатации.
   - `app-07` (APP-11, CLI `thepost admin`), `jmap-14` (JMAP-14, автонастройка клиентов), `auth-06` (AUTH-11).
   - `smtp-06` (SMTP-20…23: доверенные источники, принятые домены, точки правил, SRS/ARC) → `app-05` (APP-09).
   - `app-04` (APP-08, взаимодействие без опроса), `platform-11` (PLT-21), `ci-03`, `ci-04`, `deps-01`;
     `deps-02` (DEPS-02), `deps-03` → `jmap-15` (JMAP-22, подпись профиля) → `platform-13` (PLT-23) → `app-22` (APP-23), `admin-07` (ADM-12), `app-19` (APP-20), `app-20` (APP-21, CLI на типах tp-admin), `contracts-32` (CTR-44, список учётных записей) → `meta-29` (META-45, побайтный порядок), `docs-01` (DOC-01, PREREQUISITES), `app-23` (APP-24) и `deploy-14` (DEP-16) — база UTF-8, `deploy-15` (UTC, chrony, перевыпуск PKI), `app-24`, `sys-07`, `deploy-16` (Debian 13, Ubuntu 26.04, Astra Linux SE, РЕД ОС), `app-29` (APP-30, `cluster.listen`), `deploy-19` (DEP-23, копия OpenBao), `admin-08` (ADM-13) → `app-21` (APP-22), `platform-12` (PLT-22, mTLS источников).
   - Выход этапа: `deploy-12` (DEP-12…14 — учётные записи и наполнение стенда, отчёт по Д.7, стресс, отказы, 72 ч) и
     `deploy-13` (DEP-15 — резервное копирование SMB: pgBackRest и копия каталога писем, восстановление с `thepost verify`);
     метрики Д.7 и старт при медленном OpenBao — `meta-34`, `blob-09`, `platform-15`, `app-30`; стенд — `deploy-21` (решения 54–58).
   - Хвосты облачных волн 12.11: `app-32` (APP-35, `tls.key` в узле), `imap-09` (IMAP-18, нестабильный тест), `deploy-22` (DEP-26, юнит systemd);
     метрики Д.7 в `/metrics` узла — `contracts-37` (CTR-51, CTR-52) → `platform-17`, `meta-36` → `app-33` (APP-34) — до прогона Д.7 на `deploy-21`;
     ключ TLS межузлового канала и СУБД из OpenBao — `contracts-37` → `meta-36`, `cluster-11` → `app-34` (APP-36).
   - Хвосты PR 10–12.11: `admin-16` (ADM-23), `app-39` (CLI `--after`, OIDC device flow, остальные операции; `verify` по
     расписанию), `auth-10` (группы LDAP по `external_id`), `deploy-23` (не ждать готовности узла, Helm из ghcr.io, копии в S3)
     → `docs-02`, `platform-18` (CRL).
   - Ручная проверка подписанного профиля `.mobileconfig` на iPhone и Mac — `docs/manual-tests/JMAP-22.md` на бете (JMAP-22, APP-23).
   - Хвосты облачных волн 12.11, п. 26–55: FIX-01…03 (срочно, выше); `imap-11` (зерно фазз-тестов), `platform-19` (ACME с
     `tls.key`, брошенные ожидания соединения), `cluster-12` (смена ключа межузлового канала без перезапуска), `auth-12` и
     `admin-19` (ключ TLS LDAP, OIDC и syslog аудита из OpenBao); `platform-21` (PLT-32, доверенные подсети `ratelimit.trusted`); `contracts-51` → `smtp-24` (лимит «получателей в час»), `ci-11` → `deploy-36` (тег версии образа, закрытый ghcr).
   - Аудит 12.11 «старое рядом с новым не оставлять» (решения 93–108). Сначала — разное поведение на живом узле:
     `console-07` → `admin-22` (один API доменов); `smtp-20` (перебор AUTH — только `AbuseGuard`); `deploy-31`, `app-47` →
     `smtp-21` (локальные домены — только принятые; политика в SMTP); `contracts-46` → `meta-44` → `admin-23` (журнал аудита
     в БД, политики через контракт); `admin-24` → `sieve-06` (одна проверка Sieve, после `jmap-20`, `sieve-04`).
     Затем: `contracts-46` → `deploy-32`, `smtp-21` → `app-48`; ключи TLS только из OpenBao — `deploy-31`, `platform-19` →
     `platform-20`; `deploy-33` (только ACME, сокеты systemd, один пресет SMB) → `docs-04`; `auth-13`. GSSAPI в сборке —
     `deps-12` → `ci-10` (решение 107).
   - Реестр облачных волн, п. 62–91: FIX-05…08 (срочно, выше); `admin-25` (CLI: `--after`, остальные команды, `login --oidc`),
     `blob-10` (счётчики `verify`), `deploy-35` → `docs-05` (ключи `maintenance.*`, `cli.oidc.*`), `app-50` (OIDC с ключом
     из OpenBao), `contracts-48` (`by_external_id` обязателен).
   - Приёмка 0.2.0 сессиями (13.11, `tests-0.2.0/`): исправлены SMTP-45 (`smtp-26`, чужой отправитель) и AUTH-19
     (`auth-14`, IPv4 в счёте перебора); книга «Организация» без LDAP — `meta-41` → `app-45`. Хвосты: `platform-23` (PLT-34),
     `admin-26` (ADM-40), `contracts-55` (CTR-83, скрытие учётной записи без LDAP), `dav-13` (DAV-35, вложенные группы).
   - Хвосты 13.11: `app-59` (APP-64, POP3 сверяет сеансы после потери сигнала), `dav-11` (импорт 8-битных vCard в CardDAV;
     JMAP сырой vCard не принимает — JMAP-44 снята); этап 3 — `meta-52` (META-79, срок GC по всем шардам). Пересчёт индекса старых контактов — `meta-51`, `app-58` (влиты).
   - Хвосты 12.11, вечер: `app-57` (APP-62, приглашения из JMAP и DAV в узле, после `smtp-16`); этап 3 — `app-56` (APP-61,
     узел edge без базы, после `platform-22`), `contracts-54` (CTR-82, права отправителя через группы).
   - Приёмка этапа: критерии выхода Ф1 ниже (нагрузка Д.6 для SMB) — отчёт `criteria.md` DEP-13 без «не прошёл».
3. **Этап 3 — после пилота и шагов 4.1–4.3** («рюшечки» и Enterprise; решение 33): соответствие Exchange (`contracts-23` и зависящие: архив,
   отключённые ящики, динамические группы, личные списки, отложенная отправка, аудит доступа), внешние встречи
   (`contracts-24`, `meta-16`, `jmap-09`), модерация правилами потока (`contracts-31`, `meta-28`, `smtp-11`), шарды (`meta-13`, `meta-14`, `blob-08`), Enterprise и HA (`deploy-07`,
   `meta-25`, `cluster-09`, `app-17`), сборка Exchange (`app-08`), `contracts-30`.
   - Хвосты PR 10–12.11: шарды, реплики, Rpc и сертификаты межузлового канала в узле — `sys-08` → `app-40`; `meta-38`;
     `deploy-24` (прямые адреса и реплики СУБД, прогоны pg-ha и «малого Enterprise», отказ ВМ, OpenBao без `bootstrap.sh`,
     пресет edge); `deps-09` (mail-parser 1.0, решение 17).
   - Аудит 12.11: `cluster-13` (узел — только по URI, после `deploy-20`), `contracts-47` (ключ подписчика шарда),
     `deploy-34` (порты Helm из конфигурации); перед выпуском 1.0 — `meta-45` (миграции — одна схема, раздел Ф4).
   - Реестр облачных волн, п. 62–91: **до включения второго шарда** — `app-52` (FIX-04, GC по всем шардам); `meta-47`,
     `testkit-08` (GC по шардам); `deps-13` → `sys-09` (`statvfs` из `nix`); очередь без опроса и шард отправителя —
     `contracts-50` → `meta-48` → `smtp-23`, `mailstore-14`; `deps-09` (mail-parser 1.0 — одной волной класса B).
   - HTTP/2 между узлами, URI узла в сертификатах, PKI OpenBao (решения 31, 32, 64): `deps-06` → `cluster-10`, `deploy-20`,
     `platform-16` → `deploy-26`.
   - Синк 12.11, часть 2: сессии Admin API в PostgreSQL — `contracts-43` → `meta-40`, `admin-18` → `app-42` (решение 77).

4. **Этап 4 — Ф2 «совместная работа»** (раздел «Ф2. Совместная работа» ниже). Шаги 4.1–4.3 — раньше этапа 3
   (решение 33): волны на свободные полосы, когда в этапах 1–2 нет готовых; внутри этапа — по порядку пунктов.
   - 4.1 Календари, контакты, автоответ: `contracts-33` (CTR-45, CTR-46) → `deps-04`; `dav-01` → `dav-02` (CalDAV) → `dav-03`
     (CardDAV, GAL); `meta-30`, `meta-31`; `sieve-01` → `sieve-02`; `smtp-12`; `jmap-16` (VacationResponse, JMAP Sieve, Contacts);
     `admin-09` → `app-28` (DAV в узле), `app-25` (Sieve и автоответ в узле) → `deploy-17`.
     Хвосты: `contracts-38` (CTR-53…56) → `meta-37`, `dav-07`, `testkit-06` → `jmap-20`; `deps-08` → `sieve-04`;
     хвосты PR 10–12.11 — `contracts-40` (CTR-58, скрытие из GAL) → `auth-11`, `dav-08`;
     синк 12.11, часть 2 — GAL в `DavStore`: `contracts-41` → `meta-39`, `dav-09`, `jmap-23` → `app-41` (решение 72);
     реестр облачных волн, п. 26–55 — книга «Организация» без LDAP: `meta-41` → `app-45`; `deploy-27` (секции `maintenance`,
     `dav`); `deps-11` → `testkit-07` (корпус vCard).
     Приёмка: `docs/manual-tests/DAV-08.md`, `DAV-11.md`, `APP-29.md`, `APP-26.md` — Thunderbird, iOS и macOS, DAVx5, Postman.
   - 4.2 Управление правилами, JMAP Calendars, консоль: `sieve-03` (ManageSieve), `jmap-17` → `app-26`; `ci-05` (полоса `console`)
     → `console-01` → `console-02` → `console-03`, `deps-05` → `admin-10` (раздача консоли).
     Хвосты: `auth-08` → `app-26`; `app-35` (выгрузка схемы API); `admin-13`; `ci-06` → `admin-14` (CLI); `deps-07` → `ci-07`
     (консоль в образе); `contracts-38`, `meta-37` → `admin-15`; `sieve-05` (адрес клиента в ManageSieve).
     Синк 12.11, часть 2: `deploy-25` (образ с консолью) → `ci-07`; `deps-10` → `admin-17` (CORS, `GET /session`, `GET /orgs`,
     `operationId`) → `console-05`; `ci-09` (архив консоли в выпуске, сверка схемы на PR) — решения 68, 73, 74.
     Реестр облачных волн, п. 26–55: `deploy-28` (ManageSieve в поставке: секция и порт 4190); GSSAPI в узле — `deps-12` →
     `ci-10`, `deploy-29` → `docs-03`, `app-51` (вход GSSAPI в узле); `admin-20` (фильтры аудита, CLI блокировки); `contracts-44` → `meta-42`, `mailstore-12` →
     `admin-21`, `imap-12`, `jmap-24` (блокировка рвёт сеансы); `console-06`.
     Приёмка шага: `docs/manual-tests/UI-09.md` (консоль тремя ролями), `docs/manual-tests/AUTH-05.md` шаг 5 (GSSAPI на
     живом Kerberos, после `ci-10`).
   - 4.2б Текстовый интерфейс и единая канва управления (решение 118): `deps-14` (DEPS-14), `admin-30` (ADM-44,
     общий клиент) → `tui-01` (TUI-01, TUI-02) → `tui-02` (TUI-03, TUI-04, после `deploy-41`/DEP-55), `app-65` (TUI-05),
     `ci-14` (CI-16) → `docs-08` (DOC-08). Фирменный стиль IT Task и мышь (решение 119): `design-01` (DES-01, токены
     и контраст в CI) → `tui-03` (TUI-06, тема и пять режимов, после `tui-01`), `console-10` (UI-19, тема antd и шрифты
     без CDN); `deps-15` (DEPS-15, `ratatui-image`) → `tui-04` (TUI-07, мышь и плавный курсор, после `tui-03`). Раздел
     «Очередь»: `contracts-57` (CTR-85) → `admin-31` (ADM-45) → `console-11` (UI-20). Хвосты: `contracts-58` (CTR-86
     хранилище `ShareNotification`, CTR-87 `contacts_since` без тела по умолчанию), `app-66` (APP-69, представители в
     доставке). Подсветка вывода CLI (решение 120): `deps-16` (DEPS-16, `anstream`, `anstyle`, после `admin-30`) →
     `admin-32` (ADM-46, подсветка `thepost admin` и модуль `tp-admin-client::style`, после `design-01`) → `app-67`
     (APP-70, справка всех команд, `doctor`, `config check`).
   - 4.3 Встречи: `contracts-34` (CTR-47) → `meta-32`, `dav-04` (RFC 6638, iMIP, занятость), `smtp-13`, `jmap-18` → `app-27`;
     хвосты — `app-36` (APP-38, подключения в узле) после `dav-07`, `jmap-20`, `admin-13`, `app-26`;
     `meta-35` (META-54, разворот повторов) — после `meta-32` (META-51) и `contracts-42` (CTR-59), решение 59;
     `smtp-16` (приглашения из почтового клиента, решение 78);
     реестр облачных волн, п. 26–55: `contracts-45` → `meta-43`, `dav-10`, `jmap-25`, `smtp-18` → `app-46`.
   - Приёмка этапа: `deploy-18` (CMP-10) — критерий Ф2 на бете.
   - 4.4 «Рюшечки» Ф2 (после приёмки): `contracts-35` (общий доступ, представители, ресурсы) → `meta-33`, `auth-07`, `dav-05`,
     `jmap-19`, `admin-11` → `console-04`, `jmap-22` (тип `resource` в Principal), `smtp-19` (`SENT-BY`); `dav-06` (публикация ICS, FBURL, оргструктура в GAL) — после `contracts-23`, `contracts-24`,
     `meta-16`, `auth-05` этапа 3. Решение 121: `app-68` (APP-71, подключение общего доступа, представителей и
     бронирования в узле), `docs-09` (DOC-09) — после `dav-05`; тип ресурса — `contracts-59` (CTR-88) → `meta-54`
     (META-81), `admin-33` (ADM-47) → `console-12` (UI-21), `dav-17` (DAV-38, после `dav-05`), `jmap-28` (JMAP-45).
     Хвосты PR #760, #764, #765: `meta-55` (META-82, очередь в PgMeta) → `contracts-60` (CTR-89, без тел по умолчанию); `meta-56`
     (META-83, миграция 0042) → `jmap-29` (JMAP-46, уведомления из хранилища) → `app-69` (APP-72), `contracts-61` (CTR-90,
     вытеснение сверх предела); `dav-18` (DAV-39, удалить `router_with`); `app-70` (APP-73, e2e ответа ресурса по iMIP).
     Брендинг заказчика (решение 122): `contracts-62` (CTR-91) → `meta-57` (META-84, миграция 0043), `admin-34`
     (ADM-48, Admin API, контраст, открытый маршрут; после `design-01`, `admin-30`) → `app-72` (APP-75), `console-13`
     (UI-22, после `console-10`), `tui-05` (TUI-08, после `tui-03`), `docs-11` (DOC-11).
     Матрица доверия (решение 124, `docs/research/2026-11-14-trust-matrix.md`): `contracts-64` (CTR-93, после решения 123 —
     `x/inbound-mail-only`) → `meta-58` (META-85), `admin-35` (ADM-49) → `console-14` (UI-23), `tui-06` (TUI-09);
     `smtp-30` (SMTP-50), `dav-21` (DAV-44), `jmap-30` (JMAP-47), `sieve-07` (SIEVE-14) → `app-73` (APP-76) → `docs-12`
     (DOC-12, сверка раздела «Кто с кем и что может» в `docs/ADMIN-GUIDE.md`).
     Ответ на наше письмо и беседа (решение 125, `docs/research/2026-11-14-replies-threads.md`): `deps-18` (DEPS-18),
     `contracts-65` (CTR-94, после `contracts-64`) → `meta-59` (META-86), `mailstore-15` (MS-21), `smtp-31` (SMTP-51,
     после `smtp-30`), `jmap-31` (JMAP-48), `admin-36` (ADM-50) → `app-74` (APP-77), `deploy-43` (DEP-56) → `docs-13` (DOC-13).
     Справочник аномалий и TI (решение 126, `docs/research/2026-11-14-anomaly-ti.md`): `deps-19` (DEPS-19), `contracts-66`
     (CTR-95, после `contracts-65`) → `meta-60` (META-87), `sieve-08` (SIEVE-15, после `sieve-07`), `smtp-32` (SMTP-52,
     после `smtp-31`), `admin-37` (ADM-51) → `console-15` (UI-24), `tui-07` (TUI-10); `app-75` (APP-78, пакеты) →
     `app-76` (APP-79, источники TI), `deploy-44` (DEP-57) → `docs-14` (DOC-14).

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
| ✅ contracts-08 (ветка `x/contracts-08`) | CTR-19 | B | meta-03 | 1 |
| ✅ contracts-09 (ветка `x/contracts-09`) | CTR-20 | B | contracts-06 | 1 |
| ✅ mailstore-05 | MS-10 | A | contracts-09 | 4 |
| ✅ mailstore-06 | MS-11 | A | mailstore-02 | 4 |
| ✅ meta-07 | META-20 | A | — | 3 |
| ✅ auth-01 | AUTH-01, AUTH-02, AUTH-03, AUTH-04 | A | contracts-03 | 1 |
| ✅ search-01 | SRCH-01, SRCH-02, SRCH-03, SRCH-04 | A | contracts-03 | 3 |
| ✅ smtp-01 | SMTP-01, SMTP-02, SMTP-03, SMTP-04 | A | auth-01, testkit-02 | 4 |
| ✅ auth-02 | AUTH-05, AUTH-06 | A | auth-01, contracts-08 | 1 |
| ✅ jmap-01 | JMAP-01, JMAP-02, JMAP-03, JMAP-04 | A | auth-01, mailstore-02, testkit-02 | 3 |
| ✅ imap-01 | IMAP-01, IMAP-02, IMAP-03, IMAP-04 | A | testkit-02, auth-01, mailstore-02 | 2 |
| ✅ smtp-02 | SMTP-05, SMTP-06, SMTP-07, SMTP-08 | A | smtp-01, meta-03 | 4 |
| ✅ imap-02 | IMAP-05, IMAP-06, IMAP-07, IMAP-08 | A | imap-01 | 2 |
| ✅ search-02 | SRCH-05, SRCH-06, SRCH-07, SRCH-08 | A | search-01, meta-03 | 1 |
| ✅ admin-01 | ADM-01, ADM-02, ADM-03, ADM-04 | A | auth-01, mailstore-02 | 3 |
| ✅ smtp-03 | SMTP-09, SMTP-10, SMTP-11 | A | smtp-02 | 4 |
| ✅ jmap-02 | JMAP-05, JMAP-06, JMAP-07, JMAP-08 | A | jmap-01, smtp-02 | 3 |
| ✅ imap-03 | IMAP-09, IMAP-10, IMAP-11, IMAP-12 | A | imap-02, auth-02 | 2 |
| ✅ app-02 | APP-04, APP-05, APP-06 | A | smtp-01, imap-01, jmap-01, search-01, admin-01 | 1 |
| ✅ deploy-02 | DEP-04, DEP-05 | A | app-02 | 4 |

### Решение 2026-11-01: заимствования и базовые принципы безопасности

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-10 (ветка `x/contracts-10`) | CTR-21 | B | contracts-09 | 1 |
| ✅ sys-03 | SYS-06, SYS-07 | B | sys-01 | 2 |
| ✅ ci-03 | CI-03, CI-04 | B | ci-01 | 4 |
| ✅ platform-06 | PLT-15, PLT-16 | A | sys-03, contracts-10 | 2 |
| ✅ meta-08 | META-21, META-22, META-23, META-24 | B | contracts-10 | 3 |
| ✅ blob-06 | BLOB-13 | A | meta-08 | 4 |
| ✅ mailstore-07 | MS-12, MS-13 | A | mailstore-06 | 1 |
| ✅ smtp-04 | SMTP-12, SMTP-13, SMTP-14, SMTP-15 | A | smtp-03, contracts-13 | 4 |
| ✅ smtp-05 | SMTP-16, SMTP-17, SMTP-18, SMTP-19 | A | smtp-04, meta-08, mailstore-07 | 4 |
| ✅ auth-03 | AUTH-07 | A | auth-02, meta-08 | 1 |
| ✅ admin-02 | ADM-05, ADM-06, ADM-07 | A | admin-01, meta-08 | 3 |
| ✅ imap-04 | IMAP-13 | A | imap-03 | 2 |
| ✅ jmap-03 | JMAP-09 | A | jmap-02 | 3 |
| ✅ deploy-05 | DEP-08 | A | deploy-02, meta-08, platform-06 | 4 |

### Решение 2026-11-01: взаимодействие компонентов и требования к СУБД

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-12 (ветка `x/contracts-12`) | CTR-23 | B | contracts-11 | 1 |
| ✅ meta-09 | META-25 | A | meta-08 | 3 |
| ✅ meta-10 | META-26, META-27 | A | meta-09, contracts-12 | 3 |
| ✅ cluster-06 | CLU-09, CLU-10, CLU-11 | A | contracts-12 | 4 |
| ✅ app-04 | APP-08 | A | meta-10, cluster-06 | 2 |
| ✅ platform-07 | PLT-17 | A | platform-06 | 2 |
| ✅ mailstore-08 | MS-14 | A | mailstore-07 | 1 |
| ✅ blob-07 | BLOB-14 | A | blob-06 | 4 |
| ✅ auth-04 | AUTH-08 | A | auth-03, meta-10 | 1 |
| ✅ ci-04 | CI-05 | B | ci-03, cluster-06 | 4 |
| ✅ deploy-06 | DEP-09 | A | deploy-05, cluster-06 | 4 |

### Решение 2026-11-01: состав функций транспорта и внешний MTA

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-13 (ветка `x/contracts-13`) | CTR-24 | B | contracts-12 | 1 |
| ✅ meta-11 | META-28 | A | meta-10, contracts-13 | 3 |
| ✅ smtp-06 | SMTP-20, SMTP-21, SMTP-22, SMTP-23 | A | smtp-05, contracts-13 | 4 |
| ✅ admin-03 | ADM-08 | A | admin-02, contracts-13 | 3 |
| ✅ jmap-04 | JMAP-10 | A | jmap-03, contracts-13 | 3 |
| ✅ app-05 | APP-09 | A | meta-11, smtp-06, admin-03, jmap-04 | 2 |

### Решение 2026-11-01: масштабирование, шарды, геораспределение, администрирование — этап 3, кроме `app-07` (этап 2)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-22 (ветка `x/contracts-22`) | CTR-34 | B | contracts-12 | 1 |
| ✅ meta-13 | META-30, META-31 | A | meta-12, meta-10, contracts-22 | 3 |
| ✅ meta-14 | META-32 | A | meta-13 | 3 |
| ✅ blob-08 | BLOB-15 | A | blob-07, meta-13 | 4 |
| ✅ app-07 | APP-11 | A | admin-01 | 2 |
| ✅ deploy-07 | DEP-10 | A | deploy-06 | 4 |

### Хвосты PR 10–12.11 — этап 3: шарды, реплики, Rpc, edge, «малый Enterprise»

Источник — «Что не сделано» и отчёты PR #263, #291, #350, #369, #389, #390, #393, #398, #400, #401, #403, #406, #412.

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ sys-08 | SYS-13 | A | — | 2 |
| app-40 | APP-45, APP-46, APP-47, APP-48 | A | cluster-09, app-17, meta-13, meta-14, meta-25, sys-08 | 2 |
| ✅ meta-38 | META-61, META-62 | A | meta-13, meta-24 | 3 |
| deploy-24 | DEP-30, DEP-31, DEP-32, DEP-33, DEP-34 | A | deploy-07, deploy-15, sys-07, meta-25 | 4 |
| deps-09 (ветка `x/deps-09`) | DEPS-09 | B | deps-02 | 1 |
| cluster-13 | CLU-18 | A | deploy-20 | 4 |
| contracts-47 (ветка `x/contracts-47`) | CTR-71 | B | contracts-22 | 1 |
| deploy-34 | DEP-47 | A | deploy-07 | 4 |
| ✅ app-52 | FIX-04 | A | blob-08 | 2 |
| ✅ meta-47 | META-73 | A | blob-08 | 3 |
| ✅ testkit-08 | KIT-11 | A | blob-08 | 1 |
| ✅ deps-13 (ветка `x/deps-13`) | DEPS-13 | B | sys-08 | 1 |
| ✅ sys-09 | SYS-14 | A | deps-13 | 2 |
| ✅ contracts-50 (ветка `x/contracts-50`) | CTR-73, CTR-74, CTR-76, CTR-77, CTR-78 | B | contracts-23 | 1 |
| ✅ meta-48 | META-72, META-75 | A | contracts-50 | 3 |
| ✅ smtp-23 | SMTP-42 | A | meta-48 | 4 |
| ✅ mailstore-14 | MS-20 | A | contracts-50, mailstore-10 | 1 |

### Синк 12.11, часть 2 — этап 3: сессии Admin API в PostgreSQL, роли PKI OpenBao (решения 71, 77)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-43 (ветка `x/contracts-43`) | CTR-61 | B | contracts-29 | 1 |
| meta-40 (ветка `x/meta-40`) | META-64 | B | contracts-43 | 3 |
| admin-18 | ADM-28 | A | contracts-43 | 3 |
| app-42 | APP-50 | A | meta-40, admin-18 | 2 |
| deploy-26 | DEP-36 | A | platform-16, deploy-19 | 4 |

### Решение 2026-11-04: соответствие функциям Exchange — этап 3, кроме `jmap-14` (этап 2)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-23 (ветка `x/contracts-23`) | CTR-35 | B | contracts-22 | 1 |
| ✅ meta-15 | META-33 | A | meta-13, contracts-23 | 3 |
| ✅ auth-05 | AUTH-10 | A | auth-03, contracts-23 | 1 |
| ✅ mailstore-10 | MS-16, MS-17 | A | mailstore-08, contracts-23 | 1 |
| ✅ smtp-07 | SMTP-24, SMTP-25, SMTP-26, SMTP-27 | A | smtp-05, contracts-23 | 4 |
| ✅ jmap-08 | JMAP-15, JMAP-16, JMAP-17, JMAP-18 | A | jmap-02, contracts-23 | 3 |
| ✅ jmap-14 | JMAP-14 (этап 2) | A | jmap-02 | 3 |
| ✅ imap-06 | IMAP-15 | A | imap-02, contracts-23 | 2 |
| ✅ admin-05 | ADM-10 | A | admin-02, contracts-23 | 3 |
| app-08 | APP-12 | A | meta-15, auth-05, mailstore-10, smtp-07, jmap-08, imap-06, admin-05 | 2 |

### Решение 2026-11-04: внешние встречи — API занятости и публикации — этап 3

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| contracts-24 (ветка `x/contracts-24`) | CTR-36 | B | contracts-23 | 1 |
| meta-16 | META-34 | A | meta-15, contracts-24 | 3 |
| jmap-09 | JMAP-19, JMAP-20 | A | jmap-08, contracts-24 (и JMAP Calendars, Ф2) | 3 |

### Решение 2026-11-01: «Очистить корзину» — настройка; APPEND без правил доставки (`docs/decisions/2026-11-01-trash-purge.md`)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-14 (ветка `x/contracts-14`) | CTR-26 | B | contracts-07 | 1 |
| ✅ contracts-15 (ветка `x/contracts-15`) | CTR-27 | B | contracts-09 | 1 |
| ✅ meta-12 | META-29 | B | contracts-14, meta-03 | 3 |
| ✅ imap-05 | IMAP-14 | A | contracts-14, contracts-15, imap-02 | 2 |
| ✅ jmap-05 | JMAP-11 | A | contracts-14, contracts-15, jmap-02 | 3 |
| ✅ admin-04 | ADM-09 | A | contracts-14, admin-01 | 3 |

### Отчёт jmap-02 (PR #164)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-16 (ветка `x/contracts-16`) | CTR-28 | B | smtp-02, jmap-02 | 1 |
| ✅ contracts-17 (ветка `x/contracts-17`) | CTR-29 | B | jmap-02 | 1 |
| ✅ contracts-18 (ветка `x/contracts-18`) | CTR-30 | B | contracts-16 | 1 |
| ✅ jmap-06 | JMAP-12 | A | jmap-02 | 3 |

### Отчёты admin-02 (#174) и blob-06 (#175)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ platform-08 | PLT-18 | A | platform-06 | 2 |
| ✅ contracts-19 (ветка `x/contracts-19`) | CTR-31 | B | contracts-10, admin-02 | 1 |
| ✅ contracts-20 (ветка `x/contracts-20`) | CTR-32 | B | auth-03 | 1 |
| ✅ mailstore-09 | MS-15 | A | blob-06 | 4 |

### Остаток 2026-11-01 (отчёты imap-05, mailstore-09, app-02)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-21 (ветка `x/contracts-21`) | CTR-33 | B | contracts-14 | 1 |
| ✅ app-06 | APP-10 | A | app-02, imap-05, jmap-05, mailstore-09 | 2 |
| ✅ jmap-07 | JMAP-13 | A | jmap-02, mailstore-09 | 3 |

### Находки CMP-01: проверка клиентом Postman 2026-11-05 (приоритет)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ deploy-08 | CMP-01 | A | — | 4 |
| ✅ app-09 | CMP-02 | A | — | 2 |
| ✅ search-03 | CMP-06 | A | — | 1 |
| ✅ jmap-10 | CMP-03 | A | — | 3 |
| ✅ smtp-08 | CMP-04 | A | — | 4 |
| ✅ meta-17 | CMP-05 | A | — | 3 |
| ✅ jmap-11 | CMP-08 | A | jmap-10 | 3 |
| ✅ app-10 | CMP-07 | A | smtp-08, jmap-11, app-09 | 2 |
| ✅ testkit-04 | KIT-07 | A | meta-17 | 1 |
| ✅ meta-18 | META-35 | A | meta-17 | 3 |
| ✅ app-11 | CMP-09 | A | search-03, app-10 | 2 |
| ✅ app-12 | APP-13 | A | imap-03, platform-07, app-11 | 2 |
| ✅ platform-09 | PLT-19 | A | platform-07 (и PR #209) | 2 |
| ✅ sys-04 | SYS-08, SYS-09 | A | deploy-08, platform-07 | 2 |
| ✅ contracts-25 (ветка `x/contracts-25`) | CTR-37 | B | contracts-12 | 1 |
| ✅ imap-07 | IMAP-16 | A | imap-03 | 2 |
| ✅ app-13 | APP-14 | A | meta-09, deploy-02, app-12 | 2 |
| ✅ contracts-26 (ветка `x/contracts-26`) | CTR-38 | B | contracts-25 | 1 |
| ✅ smtp-09 | SMTP-28 | A | smtp-08 | 4 |
| ✅ meta-19 | META-36 | A | meta-10, contracts-25 (и PR #231) | 3 |
| ✅ mailstore-11 | MS-18 | A | mailstore-08, contracts-25 | 1 |
| ✅ app-14 | APP-15 | A | meta-10, app-13 | 2 |
| ✅ contracts-27 (ветка `x/contracts-27`) | CTR-39 | B | contracts-26 | 1 |
| ✅ sys-05 | SYS-10 | A | sys-04 | 2 |
| ✅ app-15 | APP-16 | A | sys-05, smtp-04, app-14 | 2 |
| ✅ contracts-28 (ветка `x/contracts-28`) | CTR-40 | B | contracts-27 | 1 |
| ✅ platform-10 | PLT-20 | A | platform-09 | 2 |
| ✅ meta-21 | META-38 | A | meta-09 | 3 |
| ✅ meta-23 | META-39 | A | contracts-28, contracts-27 | 3 |
| ✅ meta-24 | META-40 | A | meta-21 | 3 |
| ✅ smtp-10 | SMTP-29 | A | contracts-28, smtp-04 | 4 |
| ✅ cluster-07 | CLU-12 | A | cluster-06 (и PR `x/cluster-dep`) | 4 |
| ✅ cluster-08 | CLU-13 | A | cluster-06 | 4 |
| ✅ deploy-09 | DEP-11 | A | cluster-06 | 4 |
| ✅ imap-08 | IMAP-17 | A | cluster-08 | 2 |
| ✅ jmap-12 | JMAP-21 | A | cluster-08 | 3 |
| ✅ sys-06 | SYS-11 | A | sys-05 | 2 |
| ✅ platform-11 | PLT-21 | A | sys-06, cluster-08 | 2 |
| ✅ app-16 | APP-17 | A | smtp-05, auth-04, app-15 | 2 |
| ✅ app-17 | APP-18 | A | smtp-10, cluster-07, app-16 | 2 |
| ✅ meta-25 | META-41 | A | meta-21 | 3 |
| ✅ cluster-09 | CLU-14 | A | cluster-07 | 4 |
| ✅ meta-20 | META-27 (тест) | A | meta-10 | 3 |

### Ответы владельца 2026-11-10 (#193, #195, #205, #253, #272)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-29 (ветка `x/contracts-29`) | CTR-41 | B | contracts-16 | 1 |
| ✅ admin-06 | ADM-11 | A | contracts-29 | 3 |
| ✅ app-18 | APP-19 | A | admin-06 | 2 |
| ✅ meta-27 (ветка `x/admin-tokens-pg`) | META-43 | B | admin-06, app-18 | 3 |
| ✅ contracts-30 (ветка `x/contracts-30`) | CTR-42 | B | contracts-29 | 1 |
| ✅ meta-26 | META-42 | A | meta-17 | 3 |
| ✅ testkit-05 | KIT-08 | A | testkit-04 | 1 |
| ✅ auth-06 | AUTH-11 | A | auth-04 | 1 |
| ✅ deploy-10 | DEP-09 (`db_durability`, #272) | A | deploy-06 | 4 |

### Решения 2026-11-11: шарды, токен администратора, цепочка поставки, отчёты этапа 2 (`docs/decisions/2026-11-11-contracts-ci-defaults.md`, `2026-11-11-stage2-followups.md`)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ deps-01 (ветка `x/deps-01`) | DEPS-01 | B | ci-03 | 1 |
| ✅ deps-02 (ветка `x/deps-02`) | DEPS-02 | B | deps-01 | 1 |
| ✅ deps-03 (ветка `x/deps-03`) | DEPS-03 | B | jmap-14 | 1 |
| ✅ jmap-15 | JMAP-22 | A | jmap-14, deps-03 | 3 |
| ✅ admin-07 | ADM-12 | A | admin-06 | 3 |
| ✅ app-19 | APP-20 | A | auth-06, app-04 | 2 |
| ✅ app-20 | APP-21 | A | app-07, app-19 | 2 |
| ✅ contracts-32 (ветка `x/contracts-32`) | CTR-44 | B | admin-07 | 1 |
| ✅ meta-29 (ветка `x/meta-29`) | META-45 | B | contracts-32 | 3 |
| ✅ admin-08 | ADM-13 | A | admin-07 | 3 |
| ✅ app-21 | APP-22 | A | admin-08, app-20 | 2 |
| ✅ platform-13 | PLT-23 | A | platform-12 | 2 |
| ✅ app-22 | APP-23 | A | platform-13, jmap-15 | 2 |
| ✅ deploy-12 | DEP-12, DEP-13, DEP-14 | A | deploy-02, app-20 | 4 |
| ✅ deploy-13 | DEP-15 | A | deploy-02 | 4 |
| ✅ meta-34 | META-53 | A | meta-14 | 3 |
| ✅ blob-09 | BLOB-16 | A | blob-08 | 4 |
| ✅ platform-15 | PLT-25 | A | platform-13 | 2 |
| ✅ platform-21 | PLT-32 | A | — | 2 |
| ✅ app-30 | APP-31 | A | app-29 | 2 |
| ✅ deploy-21 | DEP-25 | A | deploy-12 | 4 |
| ✅ docs-01 | DOC-01 | A | — | 1 |
| ✅ app-23 | APP-24 | A | — | 2 |
| ✅ deploy-14 | DEP-16 | A | — | 4 |
| ✅ deploy-15 | DEP-17, DEP-18 | A | deploy-14 | 4 |
| ✅ app-24 | APP-25 | A | — | 2 |
| ✅ sys-07 | SYS-12 | A | — | 2 |
| ✅ deploy-16 | DEP-24, DEP-19, DEP-20 | A | deploy-13, deploy-15 | 4 |
| deploy-30 | DEP-40, DEP-41 | A | deploy-16, deploy-19 | 4 |
| ✅ app-29 | APP-30 | A | app-17 | 2 |
| ✅ deploy-19 | DEP-23 | A | deploy-13 | 4 |
| ✅ platform-12 | PLT-22 | A | smtp-06 | 2 |
| ✅ contracts-31 (ветка `x/contracts-31`) | CTR-43 | B | contracts-19, smtp-06 | 1 |
| meta-28 (ветка `x/meta-28`) | META-44 | B | contracts-31 | 3 |
| smtp-11 | SMTP-30 | A | contracts-31, meta-28 | 4 |
| ✅ contracts-36 (ветка `x/contracts-36`) | CTR-49, CTR-50 | B | contracts-23 | 1 |
| deps-06 (ветка `x/deps-06`) | DEPS-06 | B | cluster-09 | 1 |
| cluster-10 | CLU-15 | A | deps-06 | 4 |
| deploy-20 | DEP-22 | A | cluster-09 | 4 |
| ✅ platform-14 | PLT-24 | A | platform-13 | 2 |
| ✅ platform-16 | PLT-26 | A | platform-14 | 2 |

### Хвосты облачных волн 2026-11-12, этап 2: метрики Д.7 в узле, ключ TLS из OpenBao, юнит systemd, тест IMAP

Источник — разделы «Что не сделано» PR #423, #432, #433, #434, #436, #444; карточки с пометкой «Требует подтверждения владельца» —
перед запуском — ответ владельца.

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ app-32 | APP-35 | A | platform-14 | 2 |
| ✅ imap-09 | IMAP-18 | A | — | 2 |
| ✅ deploy-22 | DEP-26 | A | app-30 | 4 |
| ✅ contracts-37 (ветка `x/contracts-37`) | CTR-51, CTR-52 | B | platform-14, platform-15 | 1 |
| ✅ platform-17 | PLT-27 | A | contracts-37, platform-15 | 2 |
| ✅ meta-36 | META-55, META-56 | A | contracts-37, meta-34 | 3 |
| ✅ cluster-11 | CLU-16 | A | contracts-37 | 4 |
| ✅ app-33 | APP-34 | A | platform-17, meta-36, blob-09 | 2 |
| ✅ app-34 | APP-36 | A | meta-36, cluster-11, app-32 | 2 |

### Хвосты PR 10–12.11 — этапы 1 и 2: защита от перебора по адресу, очистка журналов, CLI, эталон SMB

Источник — «Что не сделано» и отчёты PR #212, #281, #286, #287, #293, #295, #353, #366, #375, #378, #395, #421; этап —
как у исходной волны (`app-37`, `app-38` и волны адреса клиента — этап 1, остальные — этап 2).

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-39 (ветка `x/contracts-39`) | CTR-57 | B | — | 1 |
| ✅ auth-09 | AUTH-14 | A | contracts-39 | 1 |
| ✅ smtp-15 | SMTP-35 | A | contracts-39 | 4 |
| ✅ imap-10 | IMAP-19, IMAP-20 | A | contracts-39 | 2 |
| ✅ jmap-21 | JMAP-37 | A | contracts-39 | 3 |
| ✅ app-37 | APP-39 | A | meta-27, app-16 | 2 |
| ✅ app-38 | APP-40 | A | contracts-39, auth-09 | 2 |
| ✅ admin-16 | ADM-23 | A | admin-08 | 3 |
| ✅ app-39 | APP-41, APP-42, APP-43, APP-44 | A | app-21, blob-07 | 2 |
| ✅ auth-10 | AUTH-15 | A | contracts-20 | 1 |
| ✅ deploy-23 | DEP-27, DEP-28, DEP-29 | A | deploy-19, deploy-13, ci-03 | 4 |
| ✅ docs-02 | DOC-02 | A | deploy-23 | 1 |
| ✅ platform-18 | PLT-28 | A | platform-12 | 2 |

### Хвосты облачных волн 12.11, п. 26–55 — этап 2 (FIX — срочно)

Источник — реестр недоделок облачных волн (PR #453, #479, #480, #481, #489, #490).

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ app-43 | FIX-03 | A | — | 2 |
| ✅ smtp-17 | FIX-01 | A | contracts-31 | 4 |
| ✅ app-44 | FIX-02 | A | smtp-17 | 2 |
| ✅ imap-11 | IMAP-21 | A | imap-09 | 2 |
| ✅ platform-19 | PLT-29, PLT-30 | A | platform-14, platform-17 | 2 |
| ✅ cluster-12 | CLU-17 | A | cluster-11 | 4 |
| ✅ auth-12 | AUTH-17 | A | contracts-37 | 1 |
| ✅ admin-19 | ADM-29 | A | contracts-37 | 3 |

### Аудит 12.11: старое рядом с новым — этап 2 (решения 93–108)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ console-07 | UI-16 | A | — | 1 |
| ✅ admin-22 (ветка `x/admin-22`) | ADM-33 | B | console-07 | 3 |
| ✅ smtp-20 | SMTP-41 | A | — | 4 |
| ✅ deploy-31 | DEP-42, DEP-44 | A | — | 4 |
| ✅ ci-11 (ветка `x/ci-11`) | CI-13 | B | ci-01 | 4 |
| ✅ deploy-36 | DEP-50 | A | ci-11 | 4 |
| ✅ ci-12 (ветка `x/ci-12`) | CI-14 | B | ci-11 | 4 |
| ✅ ci-13 (ветка `x/ci-13-repro`) | CI-15 | B | ci-12 | 4 |
| ✅ deploy-37 | DEP-51 | A | — | 4 |
| ✅ deploy-38 | DEP-52 | A | app-54 | 4 |
| ✅ app-55 | APP-60 | A | — | 2 |
| ✅ platform-22 | PLT-33 | A | — | 2 |
| app-56 | APP-61 | A | platform-22 | 3 |
| ✅ app-57 | APP-62 | A | smtp-16 | 2 |
| contracts-54 (ветка `x/contracts-54`) | CTR-82 | B | smtp-16 | 3 |
| ✅ meta-51 | META-78 | A | dav-07 | 2 |
| ✅ app-58 | APP-63 | A | meta-51 | 2 |
| meta-52 | META-79 | A | app-52 | 3 |
| ✅ app-59 | APP-64 | A | imap-12 | 2 |
| ✅ dav-11 | DAV-33 | A | testkit-07 | 2 |
| ✅ sys-10 | SYS-15 | A | — | 2 |
| ✅ dav-12 (ветка `x/dav-gal-arg`) | DAV-34 | B | dav-09 | 3 |
| ✅ app-60 | APP-65 | A | jmap-20 | 2 |
| ✅ smtp-26 | SMTP-45 | A | smtp-16 | 1 |
| ✅ auth-14 | AUTH-19 | A | auth-09 | 1 |
| ✅ console-08 | UI-17 | A | console-05 | 1 |
| ✅ app-62 | APP-66 | A | contracts-50 | 2 |
| ✅ app-63 | APP-67 | A | mailstore-14 | 2 |
| ✅ admin-26 | ADM-40 | A | console-08 | 3 |
| ✅ platform-23 | PLT-34 | A | auth-14 | 2 |
| ✅ contracts-55 (ветка `x/contracts-55`) | CTR-83 | B | meta-41 | 3 |
| ✅ dav-13 | DAV-35 | A | dav-10 | 4 |
| ✅ auth-15 | AUTH-20 | A | platform-23 | 1 |
| ✅ smtp-27 | SMTP-46 | A | platform-23 | 1 |
| ✅ sys-11 | SYS-16 | A | deploy-29 | 2 |
| ✅ console-09 | UI-18 | A | admin-20 | 1 |
| ✅ app-64 | APP-68 | A | sys-11 | 2 |
| ✅ deploy-39 | DEP-53 | A | sys-11 | 2 |
| ✅ docs-06 | DOC-06 | A | admin-25 | 1 |
| ✅ deploy-40 | DEP-54 | A | deploy-29 | 1 |
| ✅ deps-14 (ветка `x/deps-14`) | DEPS-14 | B | — | 1 |
| admin-30 (ветка `x/admin-30`) | ADM-44 | B | — | 1 |
| tui-01 | TUI-01, TUI-02 | A | deps-14, admin-30 | 1 |
| ✅ deploy-41 | DEP-55 | A | — | 1 |
| tui-02 | TUI-03, TUI-04 | A | tui-01, deploy-41 | 1 |
| app-65 | TUI-05 | A | tui-01 | 1 |
| ci-14 (ветка `x/ci-14`) | CI-16 | B | tui-01 | 1 |
| docs-08 | DOC-08 | A | tui-02 | 1 |
| design-01 (ветка `x/design-01`) | DES-01 | B | — | 1 |
| deps-15 (ветка `x/deps-15`) | DEPS-15 | B | deps-14 | 1 |
| tui-03 | TUI-06 | A | tui-01, design-01 | 1 |
| tui-04 | TUI-07 | A | tui-03, deps-15 | 1 |
| console-10 | UI-19 | A | design-01 | 1 |
| contracts-57 (ветка `x/contracts-57`) | CTR-85 | B | — | 1 |
| admin-31 | ADM-45 | A | contracts-57 | 1 |
| console-11 | UI-20 | A | admin-31 | 1 |
| contracts-58 (ветка `x/contracts-58`) | CTR-86, CTR-87 | B | — | 1 |
| app-66 (ветка `x/app-66`) | APP-69 | B | — | 1 |
| deps-16 (ветка `x/deps-16`) | DEPS-16 | B | admin-30 | 1 |
| admin-32 | ADM-46 | A | admin-30, design-01, deps-16 | 1 |
| app-67 | APP-70 | A | admin-32 | 1 |
| ✅ app-47 | APP-54 | A | — | 2 |
| ✅ contracts-46 (ветка `x/contracts-46`) | CTR-25, CTR-67, CTR-68, CTR-70 | B | contracts-37 | 1 |
| ✅ contracts-51 (ветка `x/contracts-51`) | CTR-79 | B | contracts-46 | 1 |
| ✅ smtp-24 | SMTP-43 | A | contracts-51 | 1 |
| ✅ smtp-25 | SMTP-44 | A | smtp-24 | 1 |
| ✅ meta-44 | META-70 | A | contracts-46 | 3 |
| ✅ admin-23 (ветка `x/admin-23`) | ADM-34, ADM-36 | A/B | meta-44, admin-22 | 3 |
| ✅ app-53 | APP-59 | A | admin-23 | 2 |
| ✅ meta-49 | META-76 | A | meta-44 | 3 |
| ✅ meta-50 | META-77 | A | — | 3 |
| admin-24 | ADM-35 | A | jmap-20 | 3 |
| ✅ sieve-06 | SIEVE-13 | A | sieve-04, jmap-20, admin-24 | 2 |
| deploy-32 | DEP-43 | A | contracts-46 | 4 |
| ✅ smtp-21 | SMTP-39, SMTP-40 | A | deploy-31, deploy-32, app-47, contracts-46, smtp-20 | 4 |
| ✅ app-48 | APP-55 | A | smtp-21 | 2 |
| ✅ platform-20 (ветка `x/platform-20`) | PLT-31 | B | deploy-31, platform-19, contracts-46 | 2 |
| ✅ deploy-33 | DEP-45, DEP-46, DEP-48 | A | platform-19, deploy-31 | 4 |
| ✅ docs-04 | DOC-04 | A | deploy-33 | 1 |
| ✅ auth-13 | AUTH-18 | A | — | 1 |

### Хвосты облачных волн 12.11, п. 62–91 — этап 2 (FIX — срочно)

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ app-49 | FIX-05 | A | — | 2 |
| ✅ mailstore-13 | FIX-06, FIX-07 | A | — | 1 |
| ✅ smtp-22 | FIX-08 | A | — | 4 |
| ✅ admin-25 | ADM-37, ADM-38, ADM-39 | A | app-39 | 3 |
| ✅ blob-10 | BLOB-17 | A | — | 4 |
| ✅ deploy-35 | DEP-49 | A | app-39, app-37 | 4 |
| ✅ docs-05 | DOC-05 | A | deploy-35 | 1 |
| ✅ app-50 | APP-56 | A | auth-12 | 2 |
| ✅ contracts-48 (ветка `x/contracts-48`) | CTR-72 | B | — | 1 |

Влиты: `app-49` — #555, `mailstore-13` — #574 (issues #475, #468 закрыты).


Первая учётная запись SMB без OIDC (#193) и отложенная отправка с отменой (#195) — решены 2026-11-10, карточки выше.

Выход Ф1: MVP SMB — критерии `docs/spec/APP-D.md#Д.7` для профиля SMB на стенде DEP-04.

## Ф2. Совместная работа (≈5 мес.)

Готовые библиотеки: iCalendar, vCard, JSCalendar, JSContact — `calcard`, XML — `quick-xml`, письма iMIP и автоответы — `mail-builder`
(всё уже в workspace, SRS 7.4); интерпретатор Sieve — свой (SRS 4.3: sieve-rs под AGPL-3.0); консоль — Refine с Ant Design (MIT),
встраивание статики — `rust-embed`. Объекты календарей и контактов — в PostgreSQL (SRS 3.1). ManageSieve — в роли `imap`.
Полоса `console` (`console/*`) появляется с `ci-05`. `jmap-09` (этап 3, `Availability/get`) ждёт `jmap-18` (JMAP Calendars).

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ contracts-33 (ветка `x/contracts-33`) | CTR-45, CTR-46 | B | — | 1 |
| ✅ deps-04 (ветка `x/deps-04`) | DEPS-04 | B | contracts-33 | 1 |
| ✅ meta-30 | META-46, META-47, META-48 | A/B | contracts-33 | 3 |
| ✅ meta-31 | META-49, META-50 | A/B | contracts-33 | 3 |
| ✅ dav-01 | DAV-01, DAV-02, DAV-03, DAV-04 | A | deps-04 | 4 |
| ✅ dav-02 | DAV-05, DAV-06, DAV-07, DAV-08 | A | dav-01 | 4 |
| ✅ dav-03 | DAV-09, DAV-10, DAV-11, DAV-12 | A | dav-01 | 4 |
| ✅ sieve-01 | SIEVE-01, SIEVE-02, SIEVE-03 | A | deps-04 | 2 |
| ✅ sieve-02 | SIEVE-04, SIEVE-05, SIEVE-06, SIEVE-07 | A | sieve-01 | 2 |
| ✅ smtp-12 | SMTP-31, SMTP-32 | A | contracts-33 | 1 |
| ✅ jmap-16 | JMAP-23, JMAP-24, JMAP-25, JMAP-26 | A | deps-04, sieve-01, dav-01 | 3 |
| ✅ admin-09 | ADM-14 | A | sieve-01 | 3 |
| ✅ app-28 | APP-29 | A | dav-02, dav-03, meta-30 | 2 |
| ✅ app-25 | APP-26 | A | sieve-02, smtp-12, meta-31, jmap-16, admin-09 | 2 |
| ✅ deploy-17 | DEP-21 | A | app-28 | 4 |
| ✅ sieve-03 | SIEVE-08, SIEVE-09 | A | sieve-02 | 2 |
| ✅ jmap-17 | JMAP-27, JMAP-28, JMAP-29 | A | jmap-16, dav-01 | 3 |
| ✅ app-26 | APP-27 | A | sieve-03, jmap-17, app-28, app-25, auth-08 | 2 |
| ✅ ci-05 (ветка `x/ci-05`) | CI-06 | B | — | 1 |
| ✅ deps-05 (ветка `x/deps-05`) | DEPS-05 | B | ci-05 | 1 |
| ✅ console-01 | UI-01, UI-02, UI-03 | A | ci-05 | 1 |
| ✅ console-02 | UI-04, UI-05, UI-06 | A | console-01 | 1 |
| ✅ console-03 | UI-07, UI-08, UI-09 | A | console-02, admin-09 | 1 |
| ✅ admin-10 | ADM-15 | A | deps-05, console-01 | 3 |
| ✅ contracts-34 (ветка `x/contracts-34`) | CTR-47 | B | contracts-33 | 1 |
| ✅ meta-35 (ветка `x/meta-35`) | META-54 | B | contracts-34, contracts-42, meta-30, meta-32 | 3 |
| ✅ smtp-14 | SMTP-34 | A | smtp-12 | 1 |
| ✅ admin-12 | ADM-17 | A | sieve-01 | 3 |
| ✅ app-31 | APP-32 | A | meta-30, meta-31 | 2 |
| ✅ meta-32 | META-51 | B | contracts-34, meta-30 | 3 |
| ✅ dav-04 | DAV-13, DAV-14, DAV-15, DAV-16 | A | dav-02, contracts-34 | 4 |
| ✅ smtp-13 | SMTP-33 | A | contracts-34, smtp-12 | 1 |
| ✅ jmap-18 | JMAP-30, JMAP-31 | A | jmap-17, contracts-34, dav-04 | 3 |
| ✅ app-27 | APP-28 | A | dav-04, smtp-13, meta-32, jmap-18, app-26 | 2 |
| deploy-18 | CMP-10 | A | app-27, console-03, deploy-17 | 4 |
| ✅ contracts-35 (ветка `x/contracts-35`) | CTR-48 | B | contracts-34 | 1 |
| meta-33 | META-52 | B | contracts-35 | 3 |
| auth-07 | AUTH-12 | A | contracts-35 | 1 |
| dav-05 | DAV-17, DAV-18, DAV-19 | A | dav-04, auth-07 | 4 |
| jmap-19 | JMAP-32, JMAP-33 | A | jmap-18, contracts-35 | 3 |
| admin-11 | ADM-16 | A | contracts-35 | 3 |
| console-04 | UI-10, UI-11 | A | console-03, admin-11 | 1 |
| dav-06 | DAV-20, DAV-21, DAV-22 | A | dav-05, contracts-23, contracts-24, meta-16, auth-05 | 4 |
| ✅ auth-08 | AUTH-13 | A | sieve-03 | 1 |
| ✅ deps-08 (ветка `x/deps-08`) | DEPS-08 | B | sieve-02 | 1 |
| ✅ sieve-04 | SIEVE-10, SIEVE-11 | A | deps-08, sieve-03 | 2 |
| ✅ testkit-06 | KIT-09 | A | — | 1 |
| ✅ app-35 | APP-37 | A | — | 2 |
| ✅ admin-13 | ADM-18, ADM-19, ADM-20 | A | admin-12 | 3 |
| ✅ ci-06 (ветка `x/ci-06`) | CI-07 | B | admin-12 | 1 |
| ✅ admin-14 | ADM-22 | A | ci-06, admin-13 | 3 |
| ✅ deps-07 (ветка `x/deps-07`) | DEPS-07 | B | deps-05 | 1 |
| ✅ ci-07 (ветка `x/ci-07`) | CI-08 | B | deps-07, admin-10, deploy-25 | 1 |
| ✅ contracts-38 (ветка `x/contracts-38`) | CTR-53, CTR-54, CTR-55, CTR-56 | B | contracts-33, contracts-31 | 1 |
| meta-37 (ветка `x/meta-37`) | META-57, META-58, META-59, META-60 | A/B | contracts-38, meta-30 | 3 |
| ✅ dav-07 | DAV-23, DAV-24, DAV-25 | A | contracts-38, dav-03 | 4 |
| ✅ jmap-20 | JMAP-34, JMAP-35, JMAP-36 | A | contracts-38, testkit-06, jmap-17, app-25 | 3 |
| ✅ admin-15 | ADM-21 | A | contracts-38, meta-37, admin-14 | 3 |
| ✅ app-36 | APP-38 | A | dav-07, meta-37, admin-13, jmap-20, app-28, app-26 | 2 |
| ✅ contracts-40 (ветка `x/contracts-40`) | CTR-58 | B | — | 1 |
| ✅ auth-11 | AUTH-16 | A | contracts-40 | 1 |
| ✅ dav-08 | DAV-26, DAV-27, DAV-28 | A | contracts-39, contracts-40, dav-03 | 4 |
| ✅ sieve-05 | SIEVE-12 | A | contracts-39, sieve-03 | 2 |
| jmap-22 | JMAP-38 | A | jmap-18, contracts-35 | 3 |
| ✅ contracts-41 (ветка `x/contracts-41`) | CTR-60 | B | contracts-40 | 1 |
| ✅ meta-39 (ветка `x/meta-39`) | META-63 | B | contracts-41, meta-37 | 3 |
| ✅ dav-09 | DAV-29 | A | contracts-41, dav-08 | 4 |
| ✅ jmap-23 | JMAP-39 | A | contracts-41, jmap-20 | 3 |
| ✅ app-41 | APP-49 | A | dav-09, meta-39, jmap-23, app-28 | 2 |
| ✅ deps-10 (ветка `x/deps-10`) | DEPS-10 | B | deps-05 | 1 |
| ✅ admin-17 | ADM-24, ADM-25, ADM-26, ADM-27 | A | admin-10, deps-10 | 3 |
| ✅ console-05 | UI-12, UI-13 | A | console-03, admin-17 | 1 |
| ✅ ci-09 (ветка `x/ci-09`) | CI-10, CI-11 | B | ci-07, app-35 | 1 |
| ✅ deploy-25 | DEP-35 | A | deps-07 | 4 |
| ✅ contracts-42 (ветка `x/contracts-42`) | CTR-59 | B | contracts-34 | 1 |
| ✅ smtp-16 | SMTP-36 | A | smtp-13, contracts-38 | 1 |
| ✅ meta-41 | META-65 | A | contracts-41 | 3 |
| ✅ app-45 | APP-51, APP-52 | A | meta-41, app-41, app-31 | 2 |
| ✅ deploy-27 | DEP-37 | A | app-31, app-28 | 4 |
| ✅ deps-11 (ветка `x/deps-11`) | DEPS-11 | B | testkit-06 | 1 |
| ✅ testkit-07 | KIT-10 | A | deps-11 | 1 |
| ✅ deploy-28 | DEP-38 | A | app-26 | 4 |
| ✅ deps-12 (ветка `x/deps-12`) | DEPS-12 | B | auth-08 | 1 |
| ✅ ci-10 (ветка `x/ci-10`) | CI-12 | B | deps-12 | 1 |
| ✅ deploy-29 | DEP-39 | A | deps-12 | 4 |
| ✅ docs-03 | DOC-03 | A | deploy-29 | 1 |
| ✅ admin-20 | ADM-30, ADM-31 | A | admin-17, admin-13, ci-06 | 3 |
| ✅ contracts-44 (ветка `x/contracts-44`) | CTR-63, CTR-64 | B | — | 1 |
| ✅ meta-42 | META-66, META-67 | A | contracts-44 | 3 |
| ✅ contracts-52 (ветка `x/contracts-52`) | CTR-80 | B | contracts-23 | 1 |
| ✅ mailstore-12 | MS-19 | A | contracts-52 | 1 |
| ✅ contracts-53 (ветка `x/contracts-53`) | CTR-81 | B | — | 3 |
| ✅ imap-13 | IMAP-23 | A | contracts-53 | 3 |
| jmap-26 | JMAP-43 | A | contracts-53 (отложена: в JMAP нет доступа к чужим ящикам — до карточки доступа) | 3 |
| ✅ admin-21 | ADM-32 | A | contracts-44, mailstore-12, admin-13 | 3 |
| ✅ imap-12 | IMAP-22 | A | contracts-44 | 2 |
| ✅ jmap-24 | JMAP-41 | A | contracts-44 | 3 |
| ✅ console-06 | UI-14, UI-15 | A | console-03, console-05, admin-13 | 1 |
| ✅ contracts-45 (ветка `x/contracts-45`) | CTR-62, CTR-65, CTR-66 | B | contracts-38 | 1 |
| ✅ meta-43 | META-68, META-69 | A | contracts-45, meta-32 | 3 |
| ✅ dav-10 | DAV-30, DAV-31, DAV-32 | A | contracts-45, dav-04 | 4 |
| ✅ jmap-25 | JMAP-40, JMAP-42 | A | dav-10, contracts-45, jmap-18 | 3 |
| ✅ smtp-18 | SMTP-37 | A | contracts-45, smtp-13 | 4 |
| ✅ app-46 | APP-53 | A | jmap-18, dav-10 | 2 |
| smtp-19 | SMTP-38 | A | smtp-13, contracts-35 | 4 |
| ✅ app-51 | APP-58 | A | deps-12 | 2 |
| app-68 | APP-71 | A | dav-05 | 2 |
| docs-09 | DOC-09 | A | dav-05 | 1 |
| contracts-59 (ветка `x/contracts-59`) | CTR-88 | B | — | 1 |
| meta-54 | META-81 | A | contracts-59 | 3 |
| admin-33 | ADM-47 | A | contracts-59, admin-30 | 3 |
| console-12 | UI-21 | A | admin-33 | 1 |
| dav-17 | DAV-38 | A | contracts-59, dav-05 | 4 |
| jmap-28 | JMAP-45 | A | contracts-59 | 3 |
| meta-55 | META-82 | A | contracts-57 | 3 |
| contracts-60 (ветка `x/contracts-60`) | CTR-89 | B | meta-55 | 1 |
| meta-56 | META-83 | A | contracts-58 | 3 |
| jmap-29 (ветка `x/jmap-29`) | JMAP-46 | B | contracts-58, meta-56 | 3 |
| app-69 | APP-72 | A | jmap-29 | 2 |
| contracts-61 (ветка `x/contracts-61`) | CTR-90 | B | contracts-58, meta-56 | 1 |
| dav-18 | DAV-39 | A | — | 4 |
| app-70 | APP-73 | A | app-68 | 2 |
| contracts-62 (ветка `x/contracts-62`) | CTR-91 | B | — | 1 |
| meta-57 | META-84 | A | contracts-62 | 3 |
| admin-34 (ветка `x/admin-34`) | ADM-48 | B | contracts-62, design-01, admin-30 | 3 |
| app-72 | APP-75 | A | meta-57, admin-34 | 2 |
| console-13 | UI-22 | A | console-10, admin-34 | 1 |
| tui-05 | TUI-08 | A | tui-03, admin-34 | 1 |
| docs-11 | DOC-11 | A | admin-34 | 1 |
| contracts-64 (ветка `x/contracts-64`) | CTR-93 | B | решение 123 (`x/inbound-mail-only`) | 1 |
| meta-58 | META-85 | A | contracts-64 | 3 |
| admin-35 (ветка `x/admin-35`) | ADM-49 | B | contracts-64, admin-30 | 3 |
| console-14 | UI-23 | A | admin-35 | 1 |
| tui-06 | TUI-09 | A | tui-02, admin-35 | 1 |
| smtp-30 | SMTP-50 | A | contracts-64, решение 123 (`x/inbound-mail-only`) | 4 |
| dav-21 | DAV-44 | A | contracts-64, решение 123 (`x/inbound-mail-only`) | 4 |
| jmap-30 | JMAP-47 | A | contracts-64 | 3 |
| sieve-07 | SIEVE-14 | A | contracts-64 | 2 |
| app-73 | APP-76 | A | meta-58, admin-35, smtp-30, dav-21, jmap-30, sieve-07 | 2 |
| docs-12 | DOC-12 | A | app-73 | 1 |
| deps-18 (ветка `x/deps-18`) | DEPS-18 | B | — | 1 |
| contracts-65 (ветка `x/contracts-65`) | CTR-94 | B | contracts-64 | 1 |
| meta-59 | META-86 | A | contracts-65 | 3 |
| mailstore-15 | MS-21 | A | contracts-65 | 1 |
| smtp-31 | SMTP-51 | A | contracts-65, deps-18, smtp-30 | 4 |
| jmap-31 | JMAP-48 | A | contracts-65, mailstore-15 | 3 |
| admin-36 (ветка `x/admin-36`) | ADM-50 | B | contracts-65, admin-30 | 3 |
| app-74 | APP-77 | A | meta-59, mailstore-15, smtp-31, jmap-31, admin-36 | 2 |
| deploy-43 | DEP-56 | A | smtp-31 | 1 |
| docs-13 | DOC-13 | A | app-74, deploy-43 | 1 |
| deps-19 (ветка `x/deps-19`) | DEPS-19 | B | — | 1 |
| contracts-66 (ветка `x/contracts-66`) | CTR-95 | B | contracts-65 | 1 |
| meta-60 | META-87 | A | contracts-66 | 3 |
| sieve-08 | SIEVE-15 | A | contracts-66, deps-19, sieve-07 | 2 |
| smtp-32 | SMTP-52 | A | contracts-66, deps-19, smtp-31 | 4 |
| admin-37 (ветка `x/admin-37`) | ADM-51 | B | contracts-66, admin-36 | 3 |
| console-15 | UI-24 | A | console-14, admin-37 | 1 |
| tui-07 | TUI-10 | A | tui-06, admin-37 | 1 |
| app-75 | APP-78 | A | meta-60, admin-37, smtp-32, sieve-08, app-74 | 2 |
| app-76 | APP-79 | A | app-75 | 2 |
| deploy-44 | DEP-57 | A | app-75, deploy-43 | 1 |
| docs-14 | DOC-14 | A | app-76, deploy-44 | 1 |

Выход Ф2: `docs/manual-tests/CMP-10.md` на бете без «не прошёл» — календари, контакты, правила и автоответы проверены клиентами
(Thunderbird, iOS и macOS, DAVx5, Evolution, Postman); консоль покрывает Admin API (`console/coverage-pending.txt` пуст, UI-09).

Строки от `auth-08` до `app-36` — хвосты облачных волн 2026-11-12 (PR #426, #427, #428, #429, #431, #439–#442, #444); карточки с
пометкой «Требует подтверждения владельца» — перед запуском — ответ владельца. Строки от `contracts-40` до `jmap-22` — хвосты
PR 10–12.11 (#293, #443, #449). Строки от `contracts-41` до `smtp-16` — синк 12.11, часть 2 (решения 68–78). Строки от
`meta-41` до `smtp-19` — реестр недоделок облачных волн 12.11, п. 26–55 (`smtp-19` — шаг 4.4). Строка `app-51` — п. 62–91 (шаг 4.2). Строки от `app-68` до `jmap-28` — решение 121 и хвосты dav-05 (#756), шаг 4.4. Строки от `contracts-62` до `docs-11` — решение 122 (брендинг заказчика), шаг 4.4. Строки от `contracts-64` до `docs-12` — решение 124 (матрица доверия), шаг 4.4; номера `contracts-63`, `smtp-29`, `dav-20` оставлены волнам решения 123. Строки от `deps-18` до `docs-13` — решение 125 (ответ на наше письмо и беседа), шаг 4.4. Строки от `deps-19` до `docs-14` — решение 126 (справочник аномалий и TI), шаг 4.4.

## Ф4. Выпуск: карточки, заведённые заранее

| Волна | Карточки | Класс | Ждёт | Поток |
|---|---|---|---|---|
| ✅ ci-08 | CI-09 | B | sieve-02 | 4 |
| meta-45 (ветка `x/meta-45`) | META-71 | B | — | 3 |

## Ф2–Ф4 базовой поставки (карточки — по завершении Ф1)

| Этап | Полосы | Содержание |
|---|---|---|
| Ф2. Совместная работа (≈5 мес.; карточки и волны — раздел выше) | dav, jmap, sieve, auth, console | CalDAV, CardDAV, GAL, scheduling, iTIP/iMIP, free/busy, ресурсы; JMAP Contacts/Calendars/Sieve; Sieve и ManageSieve; веб-консоль администратора на Refine (полоса `console`, статика вшита в бинарный файл, SRS 5.2); публикация календарей по ссылке, ссылки занятости FBURL, оргструктура в GAL (SRS 4.4) |
| Ф3. Enterprise (≈5 мес.) | smtp, blob, cluster, admin, app, deploy | Профиль S3, межузловые запросы для ролей search и admin (SRS 2.8), режим edge (без очереди, сквозная передача на transport, реплика получателей, SMTP mTLS, секреты edge — из хранилища по отдельной политике — SRS 2.6, решение 2026-11-01), пресеты transport/access/backend/mailbox, экспорт/импорт, эталонные Ansible-роли и Helm-чарт (SRS 5.6), стенд Enterprise и испытания Д.6 |
| Ф4. Выпуск (≈3 мес.) | все | Стабилизация, аудит безопасности, фаззинг парсеров, документация администратора |

## Дополнительные модули (SRS 1.3; после Ф1, порядок — решение владельца)

Каждый модуль — cargo feature + `modules.<имя>.enabled`, только через точки расширения (SRS 2.5), данные в схеме `mod_<имя>`, полоса `mod-<имя>`
(добавляется в `lanes.toml` PR класса B при старте модуля). Модули: Мультитенантность, Соответствие требованиям (политики хранения, Legal Hold,
журналирование, eDiscovery), Шардирование, Миграция бэкенда, Геораспределение (межсайтовый DR, домашняя площадка ящика), Развёртывание (GUI), Веб-клиент (без консоли администратора), Миграция с внешних систем,
Генератор нагрузки, ГОСТ-TLS, Правила потока почты, Коннектор событий (журнал событий → Kafka, NATS, вебхуки), Федерация календарей (свой протокол — issue #187, коннекторы Exchange и Microsoft 365).

## Открытые вопросы

Вопросы владельцу — [issue с меткой «решение»](https://github.com/Dicobraz421/ThePost/issues?q=is%3Aopen+label%3A%D1%80%D0%B5%D1%88%D0%B5%D0%BD%D0%B8%D0%B5). На 2026-11-11:
- #187 — собственный протокол занятости между организациями (модуль «Федерация календарей»);
- #67, #68 — проверка и тест гонки BLOB (полоса blob).
