# Задачи — карточки

Генерируется `scripts/plan_index.py` из `docs/PLAN.md` (отметки ✅ у волн) и карточек; вручную не править.
Статусы: done — волна смёржена; ready — зависимости закрыты, можно запускать; todo — ждёт зависимостей.


## Ф0. Основа (≈3 мес.)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-01](CTR-01.md) | Идентификаторы и ошибки | contracts | B | contracts-01 | done | — |
| [CTR-02](CTR-02.md) | BlobStore и BlobIndex | contracts | B | contracts-01 | done | CTR-01 |
| [CTR-03](CTR-03.md) | Метаданные ящиков и журнал изменений | contracts | B | contracts-01 | done | CTR-01 |
| [CTR-04](CTR-04.md) | EventBus, Lease, журнал событий, роли и пресеты | contracts | B | contracts-01 | done | CTR-01 |
| [SYS-01](SYS-01.md) | Сигналы процесса | sys | A | sys-01 | done | — |
| [SYS-02](SYS-02.md) | Ресурсы узла | sys | A | sys-01 | done | — |
| [SYS-03](SYS-03.md) | Сокеты слушателей | sys | A | sys-01 | done | — |
| [SYS-04](SYS-04.md) | Пул изолированных подпроцессов | sys | B | sys-01 | done | — |
| [DEP-01](DEP-01.md) | Dockerfile и compose для разработки | deploy | A | deploy-01 | done | — |
| [DEP-02](DEP-02.md) | systemd и пример конфигурации SMB | deploy | A | deploy-01 | done | — |
| [DEP-03](DEP-03.md) | Описание окружения разработчика | deploy | A | deploy-01 | done | DEP-01 |
| [CI-01](CI-01.md) | Бета и сборка aarch64 | ci | B | ci-01 | done | — |
| [CTR-05](CTR-05.md) | MailStore, SearchIndex, Directory, Auth | contracts | B | contracts-02 | done | CTR-02,CTR-03 |
| [CTR-10](CTR-10.md) | Точки расширения и модули | contracts | B | contracts-02 | done | CTR-04,CTR-05 |
| [KIT-01](KIT-01.md) | In-memory BlobStore и BlobIndex | testkit | A | testkit-01 | done | CTR-02 |
| [KIT-02](KIT-02.md) | In-memory метаданные | testkit | A | testkit-01 | done | CTR-03 |
| [KIT-03](KIT-03.md) | In-memory шина, лизы, журнал событий, часы | testkit | A | testkit-01 | done | CTR-04 |
| [KIT-04](KIT-04.md) | Корпус писем и хелперы тестов | testkit | A | testkit-01 | done | — |
| [PLT-01](PLT-01.md) | Загрузка конфигурации | platform | A | platform-01 | done | CTR-04 |
| [PLT-02](PLT-02.md) | Hot reload конфигурации | platform | A | platform-01 | done | PLT-01,SYS-01 |
| [PLT-03](PLT-03.md) | Телеметрия | platform | A | platform-01 | done | — |
| [PLT-04](PLT-04.md) | Служебный HTTP: метрики и health | platform | A | platform-01 | done | PLT-03,CTR-04 |
| [META-01](META-01.md) | Схема БД v1 и миграции | meta | B | meta-01 | done | CTR-03 |
| [META-02](META-02.md) | BlobIndex на PostgreSQL | meta | A | meta-01 | done | META-01,CTR-02 |
| [META-03](META-03.md) | AccountStore и MailboxStore | meta | A | meta-01 | done | META-01 |
| [META-04](META-04.md) | MessageStore и ChangeLog | meta | A | meta-01 | done | META-03 |
| [CLU-01](CLU-01.md) | Шина событий в процессе | cluster | A | cluster-01 | done | CTR-04 |
| [CLU-02](CLU-02.md) | Консистентное хэширование | cluster | A | cluster-01 | done | — |
| [CLU-03](CLU-03.md) | Шина событий Valkey | cluster | A | cluster-01 | done | CLU-01 |
| [BLOB-01](BLOB-01.md) | Локальный BlobStore | blob | A | blob-01 | done | CTR-02 |
| [BLOB-02](BLOB-02.md) | S3 BlobStore | blob | A | blob-01 | done | CTR-02 |
| [BLOB-04](BLOB-04.md) | NVMe-кэш объектов | blob | A | blob-01 | done | BLOB-01 |
| [PLT-05](PLT-05.md) | Рантайм ролей | platform | A | platform-02 | done | PLT-02,PLT-04 |
| [PLT-06](PLT-06.md) | Слушатели TCP: PROXY v2 и TLS | platform | A | platform-02 | done | SYS-03 |
| [PLT-07](PLT-07.md) | Rate limit | platform | A | platform-02 | done | PLT-01 |
| [PLT-08](PLT-08.md) | ACME-клиент (SMB) | platform | A | platform-02 | done | PLT-06 |
| [META-05](META-05.md) | LeaseStore | meta | A | meta-02 | done | META-01,CTR-04 |
| [META-06](META-06.md) | Журнал событий ядра | meta | A | meta-02 | done | META-01,META-05,CTR-04 |
| [META-07](META-07.md) | Пул соединений и проверки СУБД | meta | A | meta-02 | done | META-01 |
| [META-08](META-08.md) | Справочник шардов | meta | A | meta-02 | done | META-01 |
| [META-13](META-13.md) | Миграции модулей | meta | A | meta-02 | done | META-01,CTR-10 |
| [BLOB-03](BLOB-03.md) | BlobWriter: протокол записи | blob | A | blob-02 | done | CTR-02,KIT-01 |
| [BLOB-05](BLOB-05.md) | GC-воркер | blob | A | blob-02 | done | BLOB-03,KIT-03 |
| [BLOB-06](BLOB-06.md) | Поиск «сирот» | blob | A | blob-02 | done | BLOB-05 |
| [BLOB-07](BLOB-07.md) | Verify | blob | A | blob-02 | done | BLOB-06 |
| [BLOB-08](BLOB-08.md) | Тест гонки GC × дедупликация | blob | A | blob-02 | done | BLOB-05 |
| [MS-01](MS-01.md) | Модель «скелет + части» | mailstore | A | mailstore-01 | done | CTR-05,KIT-04 |
| [MS-02](MS-02.md) | Приём письма (ingest) | mailstore | A | mailstore-01 | done | MS-01,KIT-01,KIT-02,KIT-03,CTR-10 |
| [MS-03](MS-03.md) | Выдача письма | mailstore | A | mailstore-01 | done | MS-02 |
| [MS-04](MS-04.md) | Флаги, копирование, перемещение | mailstore | A | mailstore-01 | done | MS-02 |
| [APP-01](APP-01.md) | Сборка ролей по конфигурации | app | A | app-01 | done | PLT-05,PLT-09,META-04,META-13,BLOB-03,MS-02,CLU-01 |
| [APP-02](APP-02.md) | Команда doctor | app | A | app-01 | done | APP-01,META-07 |
| [APP-03](APP-03.md) | Команда verify | app | A | app-01 | done | APP-01,BLOB-07 |

## Ф0 · Решения владельца 2026-10-30 (`docs/decisions/2026-10-30-owner-answers.md`)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-11](CTR-11.md) | tokio, tokio-util и serde_json в контрактах | contracts | B | contracts-04 | done | CTR-10 |
| [CTR-12](CTR-12.md) | TLS внутренних каналов и неизменяемость сегментов в контракте | contracts | B | contracts-04 | done | CTR-10 |
| [SYS-05](SYS-05.md) | serde и tracing в tp-sys | sys | B | sys-02 | done | SYS-04 |
| [CI-02](CI-02.md) | Valkey в CI | ci | B | ci-02 | done | CI-01 |
| [DEP-06](DEP-06.md) | бакет S3 и TLS в compose | deploy | A | deploy-03 | done | DEP-01 |
| [CLU-04](CLU-04.md) | шина Valkey только по TLS | cluster | A | cluster-02 | done | CLU-01,CTR-12 |
| [CLU-05](CLU-05.md) | счётчики шины для метрик | cluster | A | cluster-02 | done | CLU-01 |
| [PLT-09](PLT-09.md) | строгая и мягкая проверка конфигурации | platform | A | platform-03 | done | PLT-02 |
| [PLT-10](PLT-10.md) | метрики шины в общем реестре | platform | A | platform-03 | done | CLU-05,PLT-03 |
| [PLT-11](PLT-11.md) | TLS служебного HTTP | platform | A | platform-03 | done | PLT-06,CTR-12 |
| [META-14](META-14.md) | PostgreSQL только по TLS | meta | A | meta-04 | done | META-07,CTR-12 |
| [BLOB-09](BLOB-09.md) | S3 только по https | blob | A | blob-03 | done | BLOB-02,CTR-12 |
| [BLOB-10](BLOB-10.md) | кэш сегментов индексов | blob | A | blob-03 | done | BLOB-04,CTR-12 |

## Ф0 · Решения владельца 2026-10-30, секреты и журнал (`docs/decisions/2026-10-30-config-events-secrets.md`)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-13](CTR-13.md) | длительности humantime в секциях конфигурации | contracts | B | contracts-05 | done | CTR-10 |
| [CTR-14](CTR-14.md) | членство узлов и назначение разделов журнала | contracts | B | contracts-05 | done | CTR-10 |
| [CTR-15](CTR-15.md) | хранилище секретов и ссылки на секреты | contracts | B | contracts-05 | done | CTR-10 |
| [DEP-07](DEP-07.md) | OpenBao в эталонной поставке, роль приложения без суперпользователя | deploy | A | deploy-04 | done | DEP-06 |
| [KIT-06](KIT-06.md) | MemSecretStore и MemMembership | testkit | A | testkit-03 | done | CTR-14,CTR-15 |
| [META-15](META-15.md) | секции конфигурации tp-meta из config.yaml | meta | B | meta-05 | done | CTR-13,META-07 |
| [META-16](META-16.md) | членство узлов в PostgreSQL, разделы журнала по назначению | meta | A | meta-05 | done | CTR-14,META-06 |
| [META-17](META-17.md) | учётные данные БД и шардов из хранилища секретов | meta | A | meta-05 | done | CTR-15,META-08 |
| [CLU-06](CLU-06.md) | назначение разделов журнала по живым узлам | cluster | A | cluster-03 | done | CTR-14,CLU-02 |
| [PLT-12](PLT-12.md) | клиент OpenBao/Vault, секреты в конфигурации | platform | B | platform-04 | done | CTR-15,CTR-12,PLT-09 |
| [PLT-13](PLT-13.md) | длительности tp-platform — строки humantime | platform | A | platform-04 | done | PLT-06,CTR-13 |
| [META-18](META-18.md) | динамические учётные данные PostgreSQL | meta | A | meta-06 | done | META-17,PLT-12 |
| [META-19](META-19.md) | перечисление учёта BLOB для verify | meta | A | meta-06 | done | META-04 |

## Ф0 · Остаток связки Ф0 (отчёт app-01, PR #89; конверт ingest — `docs/decisions/2026-10-31-ingest-envelope.md`)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [BLOB-11](BLOB-11.md) | конфиг tp-blob из config.yaml, ключи S3 из хранилища секретов | blob | A | blob-04 | done | BLOB-09,CTR-13,CTR-15 |
| [CLU-07](CLU-07.md) | конфиг шины из config.yaml | cluster | A | cluster-04 | done | CLU-04,CLU-06,CTR-13,CTR-15 |
| [PLT-14](PLT-14.md) | клиент OpenBao на reqwest | platform | A | platform-05 | done | PLT-12 |
| [CTR-16](CTR-16.md) | конверт письма и пересылка в контракте ingest | contracts | B | contracts-06 | done | CTR-05 |
| [MS-08](MS-08.md) | ingest — отправитель с конверта, пересылка копий | mailstore | A | mailstore-03 | done | MS-02,CTR-16 |
| [APP-07](APP-07.md) | хранилище секретов и остаток связки Ф0 | app | A | app-03 | done | APP-01,PLT-12,PLT-13,BLOB-11,CLU-07,META-19 |
| [BLOB-12](BLOB-12.md) | убрать совместимые поля конфига tp-blob | blob | A | blob-05 | done | BLOB-11,APP-07 |
| [CLU-08](CLU-08.md) | убрать ValkeyBusConfig и ValkeyBus::start | cluster | A | cluster-05 | done | CLU-07,APP-07 |

## Ф1. MVP SMB (≈6 мес.)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-06](CTR-06.md) | Очередь SMTP | contracts | B | contracts-03 | done | CTR-05 |
| [CTR-07](CTR-07.md) | Корзина и квоты | contracts | B | contracts-03 | done | CTR-05 |
| [CTR-08](CTR-08.md) | SASL-интерфейс | contracts | B | contracts-03 | done | CTR-05 |
| [CTR-09](CTR-09.md) | IndexManifestStore и перестройка индекса | contracts | B | contracts-03 | done | CTR-05 |
| [KIT-05](KIT-05.md) | Фейки Ф1: SASL, Authenticator, Directory, поиск, очередь, корзина | testkit | A | testkit-02 | done | CTR-06,CTR-07,CTR-08,CTR-09 |
| [META-09](META-09.md) | QueueStore на PostgreSQL | meta | B | meta-03 | done | CTR-06,META-05 |
| [META-10](META-10.md) | Корзина, квоты, политики на PostgreSQL | meta | B | meta-03 | done | CTR-07,META-04 |
| [META-11](META-11.md) | ACL на PostgreSQL | meta | B | meta-03 | done | CTR-05,META-03 |
| [META-12](META-12.md) | Манифесты индексов | meta | B | meta-03 | done | CTR-09,META-01 |
| [MS-05](MS-05.md) | Корзина и истечение | mailstore | A | mailstore-02 | done | CTR-07,MS-04 |
| [MS-06](MS-06.md) | Квоты | mailstore | A | mailstore-02 | done | CTR-07,MS-02 |
| [MS-07](MS-07.md) | Метаданные для протоколов | mailstore | A | mailstore-02 | done | MS-01 |
| [CTR-18](CTR-18.md) | окончательное удаление — только из корзины | contracts | B | contracts-07 | done | CTR-07 |
| [MS-09](MS-09.md) | тесты корзины и квот на фейках testkit | mailstore | A | mailstore-04 | done | MS-05,KIT-05 |
| [CTR-19](CTR-19.md) | контракт AclStore (права и делегирование) | contracts | B | contracts-08 | done | CTR-05,META-12 |
| [CTR-20](CTR-20.md) | MessageStore::insert_batch — все доставки одной транзакцией | contracts | B | contracts-09 | done | CTR-16 |
| [MS-10](MS-10.md) | ingest — раскладка одной транзакцией, без отката | mailstore | A | mailstore-05 | done | CTR-20 |
| [MS-11](MS-11.md) | lines у частей вложенного message/rfc822 | mailstore | A | mailstore-06 | done | MS-07 |
| [META-20](META-20.md) | нестабильный pg_lease second_holder_waits_for_expiry | meta | A | meta-07 | done | — |
| [AUTH-01](AUTH-01.md) | Синхронизация LDAP | auth | A | auth-01 | done | CTR-05 |
| [AUTH-02](AUTH-02.md) | Пароли | auth | A | auth-01 | done | CTR-05 |
| [AUTH-03](AUTH-03.md) | OIDC Bearer | auth | A | auth-01 | done | CTR-05 |
| [AUTH-04](AUTH-04.md) | SASL-механизмы | auth | A | auth-01 | done | CTR-08,AUTH-02,AUTH-03 |
| [SRCH-01](SRCH-01.md) | Схема индекса и менеджер | search | A | search-01 | done | CTR-09 |
| [SRCH-02](SRCH-02.md) | Индексатор | search | A | search-01 | done | SRCH-01,KIT-03 |
| [SRCH-03](SRCH-03.md) | Исполнитель запросов | search | A | search-01 | done | SRCH-01 |
| [SRCH-04](SRCH-04.md) | Анализаторы | search | A | search-01 | done | SRCH-01 |
| [SMTP-01](SMTP-01.md) | SMTP-сервер: сессия | smtp | A | smtp-01 | done | PLT-06 |
| [SMTP-02](SMTP-02.md) | Роль submission и TLS | smtp | A | smtp-01 | done | SMTP-01,AUTH-04,KIT-05 |
| [SMTP-03](SMTP-03.md) | Локальная доставка | smtp | A | smtp-01 | done | SMTP-01,MS-02 |
| [SMTP-04](SMTP-04.md) | SPF, DKIM, DMARC, ARC | smtp | A | smtp-01 | done | SMTP-03 |
| [AUTH-05](AUTH-05.md) | Kerberos GSSAPI | auth | A | auth-02 | done | AUTH-04 |
| [AUTH-06](AUTH-06.md) | Авторизация и делегирование | auth | A | auth-02 | done | CTR-05,AUTH-01,CTR-19 |
| [JMAP-01](JMAP-01.md) | Ядро JMAP | jmap | A | jmap-01 | done | AUTH-03,PLT-06,KIT-05,CTR-10 |
| [JMAP-02](JMAP-02.md) | Mailbox | jmap | A | jmap-01 | done | JMAP-01,MS-04 |
| [JMAP-03](JMAP-03.md) | Email/get и Thread | jmap | A | jmap-01 | done | JMAP-02,MS-03,MS-07 |
| [JMAP-04](JMAP-04.md) | Email/query | jmap | A | jmap-01 | done | JMAP-03 |
| [IMAP-01](IMAP-01.md) | Парсер и сериализатор IMAP4rev2 | imap | A | imap-01 | done | — |
| [IMAP-02](IMAP-02.md) | Сессия и аутентификация | imap | A | imap-01 | done | IMAP-01,PLT-06,AUTH-04,KIT-05 |
| [IMAP-03](IMAP-03.md) | Операции с ящиками | imap | A | imap-01 | done | IMAP-02,MS-04 |
| [IMAP-04](IMAP-04.md) | FETCH | imap | A | imap-01 | done | IMAP-03,MS-03,MS-07 |
| [SMTP-05](SMTP-05.md) | Milter-клиент | smtp | A | smtp-02 | done | SMTP-03 |
| [SMTP-06](SMTP-06.md) | Очередь исходящей почты | smtp | A | smtp-02 | done | SMTP-02,META-09 |
| [SMTP-07](SMTP-07.md) | DSN | smtp | A | smtp-02 | done | SMTP-06 |
| [SMTP-08](SMTP-08.md) | DKIM-подпись | smtp | A | smtp-02 | done | SMTP-06 |
| [IMAP-05](IMAP-05.md) | STORE и CONDSTORE | imap | A | imap-02 | done | IMAP-04 |
| [IMAP-06](IMAP-06.md) | SEARCH | imap | A | imap-02 | done | IMAP-04 |
| [IMAP-07](IMAP-07.md) | APPEND, COPY, MOVE, EXPUNGE | imap | A | imap-02 | done | IMAP-04,MS-05 |
| [IMAP-08](IMAP-08.md) | IDLE | imap | A | imap-02 | done | IMAP-04,CLU-01 |
| [SRCH-05](SRCH-05.md) | Сегменты в BlobStore | search | A | search-02 | done | SRCH-02,META-12 |
| [SRCH-06](SRCH-06.md) | Кэш индексов | search | A | search-02 | done | SRCH-05 |
| [SRCH-07](SRCH-07.md) | Извлечение текста вложений | search | A | search-02 | done | SYS-04,SRCH-02 |
| [SRCH-08](SRCH-08.md) | Перестройка индекса ящика | search | A | search-02 | done | SRCH-02 |
| [ADM-01](ADM-01.md) | Каркас Admin API | admin | A | admin-01 | done | AUTH-03 |
| [ADM-02](ADM-02.md) | Организация, домены, учётки, квоты | admin | A | admin-01 | done | ADM-01,MS-06 |
| [ADM-03](ADM-03.md) | Корзина | admin | A | admin-01 | done | ADM-01,MS-05 |
| [ADM-04](ADM-04.md) | Пароли приложений | admin | A | admin-01 | done | ADM-01,AUTH-02 |
| [SMTP-09](SMTP-09.md) | Доставка наружу | smtp | A | smtp-03 | done | SMTP-06 |
| [SMTP-10](SMTP-10.md) | MTA-STS и DANE | smtp | A | smtp-03 | done | SMTP-09 |
| [SMTP-11](SMTP-11.md) | Лимиты отправки и метрики | smtp | A | smtp-03 | done | SMTP-06 |
| [JMAP-05](JMAP-05.md) | Email/set, import, copy | jmap | A | jmap-02 | done | JMAP-03,MS-05 |
| [JMAP-06](JMAP-06.md) | Бинарные данные | jmap | A | jmap-02 | done | JMAP-01,MS-03 |
| [JMAP-07](JMAP-07.md) | Identity и EmailSubmission | jmap | A | jmap-02 | done | JMAP-05,SMTP-06 |
| [JMAP-08](JMAP-08.md) | Push и SearchSnippet | jmap | A | jmap-02 | done | JMAP-04,CLU-01 |
| [IMAP-09](IMAP-09.md) | QRESYNC | imap | A | imap-03 | done | IMAP-05 |
| [IMAP-10](IMAP-10.md) | NAMESPACE, QUOTA, ACL | imap | A | imap-03 | done | IMAP-03,AUTH-06,MS-06 |
| [IMAP-11](IMAP-11.md) | COMPRESS и мелкие расширения | imap | A | imap-03 | done | IMAP-02 |
| [IMAP-12](IMAP-12.md) | POP3 | imap | A | imap-03 | done | IMAP-04 |
| [APP-04](APP-04.md) | Профиль SMB | app | A | app-02 | done | APP-01,SMTP-04,IMAP-04,JMAP-04,SRCH-02,ADM-01 |
| [APP-05](APP-05.md) | Сквозной тест MVP | app | A | app-02 | done | APP-04 |
| [APP-06](APP-06.md) | doctor: DNS и сертификаты | app | A | app-02 | done | APP-02 |
| [DEP-04](DEP-04.md) | Стенд SMB и базовая нагрузка | deploy | A | deploy-02 | done | APP-05 |
| [DEP-05](DEP-05.md) | Эталонная установка SMB одной командой | deploy | A | deploy-02 | done | DEP-02 |

## Ф1 · Решение 2026-11-01: заимствования и базовые принципы безопасности

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-21](CTR-21.md) | контракты: сжатие BLOB, журнал доставки, уровни нагрузки, группы рассылки | contracts | B | contracts-10 | done | CTR-20 |
| [SYS-06](SYS-06.md) | ресурсы узла по cgroups с перечитыванием | sys | A | sys-03 | done | SYS-02 |
| [SYS-07](SYS-07.md) | seccomp и Landlock по ролям | sys | B | sys-03 | done | SYS-04,SYS-06 |
| [CI-03](CI-03.md) | цепочка поставки: cargo-audit, подпись, воспроизводимость | ci | B | ci-03 | done | CI-01 |
| [CI-04](CI-04.md) | фаззинг в CI | ci | B | ci-03 | done | CI-01 |
| [PLT-15](PLT-15.md) | поведение под нагрузкой | platform | A | platform-06 | done | SYS-06,CTR-21,PLT-05 |
| [PLT-16](PLT-16.md) | минимальные привилегии при запуске | platform | A | platform-06 | done | SYS-07,PLT-06 |
| [META-21](META-21.md) | группы рассылки в PostgreSQL | meta | B | meta-08 | done | CTR-21,META-11 |
| [META-22](META-22.md) | журнал доставки и журнал аудита только на добавление | meta | B | meta-08 | done | CTR-21,META-13 |
| [META-23](META-23.md) | раздельные учётные записи PostgreSQL | meta | B | meta-08 | done | META-13,META-17 |
| [META-24](META-24.md) | признак сжатия в учёте BLOB | meta | B | meta-08 | done | CTR-21,META-02 |
| [BLOB-13](BLOB-13.md) | сжатие объектов zstd | blob | A | blob-06 | done | META-24 |
| [MS-12](MS-12.md) | предупреждения о заполнении квоты | mailstore | A | mailstore-07 | done | MS-06 |
| [MS-13](MS-13.md) | фаззинг разбора MIME | mailstore | A | mailstore-07 | done | MS-11 |
| [SMTP-12](SMTP-12.md) | экран соединений | smtp | A | smtp-04 | done | SMTP-01 |
| [SMTP-13](SMTP-13.md) | адресация: catch-all, подадреса, переписывание | smtp | A | smtp-04 | done | SMTP-03,AUTH-01 |
| [SMTP-14](SMTP-14.md) | планировщик исходящей очереди по доменам | smtp | A | smtp-04 | done | SMTP-06,SMTP-09 |
| [SMTP-15](SMTP-15.md) | политики по доменам назначения | smtp | A | smtp-04 | done | SMTP-09,SMTP-10,CTR-24 |
| [SMTP-16](SMTP-16.md) | проверки до 250: квота, группы рассылки | smtp | A | smtp-05 | done | SMTP-13,MS-12,META-21 |
| [SMTP-17](SMTP-17.md) | запись журнала доставки | smtp | A | smtp-05 | done | SMTP-03,SMTP-09,META-22 |
| [SMTP-18](SMTP-18.md) | обучение антиспама по перемещению в «Спам» | smtp | A | smtp-05 | done | SMTP-05 |
| [SMTP-19](SMTP-19.md) | фаззинг разборщиков SMTP | smtp | A | smtp-05 | done | SMTP-01 |
| [AUTH-07](AUTH-07.md) | группы рассылки из каталога | auth | A | auth-03 | done | AUTH-01,META-21 |
| [ADM-05](ADM-05.md) | поиск в журнале доставки | admin | A | admin-02 | done | ADM-01,META-22 |
| [ADM-06](ADM-06.md) | роли оператора и аудитора, экспорт аудита | admin | A | admin-02 | done | ADM-01 |
| [ADM-07](ADM-07.md) | группы рассылки: модерация и настройки | admin | A | admin-02 | done | ADM-01,META-21 |
| [IMAP-13](IMAP-13.md) | фаззинг разборщика IMAP | imap | A | imap-04 | done | IMAP-01 |
| [JMAP-09](JMAP-09.md) | фаззинг разборщика JMAP | jmap | A | jmap-03 | done | JMAP-01 |
| [DEP-08](DEP-08.md) | минимальные привилегии в эталонной поставке | deploy | A | deploy-05 | done | DEP-02,DEP-07,META-23,PLT-16 |

## Ф1 · Решение 2026-11-01: взаимодействие компонентов и требования к СУБД

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-23](CTR-23.md) | контракты взаимодействия: сигналы, Rpc, NodeInfo, AbuseBlocks | contracts | B | contracts-12 | done | CTR-22 |
| [META-25](META-25.md) | поиск primary и поведение при переключении СУБД | meta | A | meta-09 | done | META-02 |
| [META-26](META-26.md) | сигналы через LISTEN/NOTIFY | meta | A | meta-10 | done | CTR-23,META-25 |
| [META-27](META-27.md) | состав кластера с ролями и блокировки | meta | A | meta-10 | done | CTR-23,META-16 |
| [CLU-09](CLU-09.md) | удалить шину Valkey | cluster | A | cluster-06 | done | CTR-23 |
| [CLU-10](CLU-10.md) | состав кластера и разделы по сигналу | cluster | A | cluster-06 | done | CTR-23 |
| [CLU-11](CLU-11.md) | запросы узел → узел (Rpc) | cluster | A | cluster-06 | done | CTR-23 |
| [APP-08](APP-08.md) | сборка взаимодействия без опроса | app | A | app-04 | done | META-26,CLU-09,CLU-10 |
| [PLT-17](PLT-17.md) | перечитывание конфигурации и сертификатов по событиям | platform | A | platform-07 | done | PLT-16 |
| [MS-14](MS-14.md) | истечение корзины по ближайшему сроку | mailstore | A | mailstore-08 | done | MS-13 |
| [BLOB-14](BLOB-14.md) | GC по ближайшему сроку отсрочки | blob | A | blob-07 | done | BLOB-13 |
| [AUTH-08](AUTH-08.md) | блокировки перебора на всех узлах | auth | A | auth-04 | done | META-27 |
| [CI-05](CI-05.md) | CI без Valkey, проверка отсутствия интерпретаторов | ci | B | ci-04 | done | CLU-09 |
| [DEP-09](DEP-09.md) | стенды без Valkey, эталоны HA СУБД | deploy | A | deploy-06 | done | CLU-09 |

## Ф1 · Решение 2026-11-01: состав функций транспорта и внешний MTA

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-24](CTR-24.md) | контракты настроек транспорта и точка расширения правил потока | contracts | B | contracts-13 | done | CTR-23 |
| [META-28](META-28.md) | хранение настроек транспорта | meta | A | meta-11 | done | CTR-24,META-26 |
| [SMTP-20](SMTP-20.md) | доверенные источники: шлюз и приложения | smtp | A | smtp-06 | done | CTR-24,SMTP-12,SMTP-04 |
| [SMTP-21](SMTP-21.md) | принятые домены и внутренняя ретрансляция | smtp | A | smtp-06 | done | CTR-24,SMTP-03,SMTP-09 |
| [SMTP-22](SMTP-22.md) | точки вызова правил потока почты | smtp | A | smtp-06 | done | CTR-24,SMTP-05,SMTP-16,SMTP-17 |
| [SMTP-23](SMTP-23.md) | SRS и ARC-подпись при пересылке | smtp | A | smtp-06 | done | SMTP-08,SMTP-13 |
| [ADM-08](ADM-08.md) | Admin API настроек транспорта | admin | A | admin-03 | done | ADM-01,CTR-24 |
| [JMAP-10](JMAP-10.md) | правила потока в EmailSubmission | jmap | A | jmap-04 | done | JMAP-07,CTR-24 |
| [APP-09](APP-09.md) | сборка настроек транспорта и точки правил потока | app | A | app-05 | done | META-28,SMTP-22,JMAP-10,ADM-08 |

## Ф1 · Решение 2026-11-01: масштабирование, шарды, геораспределение, администрирование — этап 3, кроме `app-07` (этап 2)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-34](CTR-34.md) | раздел журнала событий по шардам | contracts | B | contracts-22 | done | CTR-23 |
| [META-30](META-30.md) | хранилища ящика через справочник шардов | meta | A | meta-13 | ready | META-29,META-27 |
| [META-31](META-31.md) | журнал событий, очередь и учёт BLOB по шардам | meta | A | meta-13 | ready | META-30,CTR-34 |
| [META-32](META-32.md) | чтение с реплик для фоновых задач | meta | A | meta-14 | todo | META-30 |
| [BLOB-15](BLOB-15.md) | сборка мусора по учёту ссылок всех шардов | blob | A | blob-08 | todo | META-31,BLOB-14 |
| [APP-11](APP-11.md) | CLI thepost admin | app | A | app-07 | done | ADM-01 |
| [DEP-10](DEP-10.md) | HPA, VPA и эталон «малый Enterprise» | deploy | A | deploy-07 | ready | DEP-09 |

## Ф1 · Решение 2026-11-04: соответствие функциям Exchange — этап 3, кроме `jmap-14` (этап 2)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-35](CTR-35.md) | контракты: соответствие Exchange | contracts | B | contracts-23 | ready | CTR-34 |
| [META-33](META-33.md) | хранение: списки, контакты, оргструктура, аудит, отключённые ящики | meta | A | meta-15 | todo | CTR-35,META-30 |
| [AUTH-10](AUTH-10.md) | синхронизация каталога: контакты, оргструктура, отключение ящиков | auth | A | auth-05 | todo | CTR-35,AUTH-07 |
| [MS-16](MS-16.md) | архив ящика | mailstore | A | mailstore-10 | todo | CTR-35,MS-14 |
| [MS-17](MS-17.md) | хранение отключённых ящиков | mailstore | A | mailstore-10 | todo | CTR-35,MS-16 |
| [SMTP-24](SMTP-24.md) | отложенная отправка и отмена | smtp | A | smtp-07 | todo | CTR-35,SMTP-14 |
| [SMTP-25](SMTP-25.md) | личные списки отправителей при доставке | smtp | A | smtp-07 | todo | CTR-35,SMTP-16 |
| [SMTP-26](SMTP-26.md) | раскрытие динамических групп и контактов | smtp | A | smtp-07 | todo | CTR-35,SMTP-16 |
| [SMTP-27](SMTP-27.md) | профиль Milter для исходящей почты | smtp | A | smtp-07 | todo | SMTP-05 |
| [JMAP-15](JMAP-15.md) | отложенная отправка в EmailSubmission | jmap | A | jmap-08 | todo | CTR-35,JMAP-07 |
| [JMAP-16](JMAP-16.md) | статус доставки EmailSubmission | jmap | A | jmap-08 | todo | CTR-35,JMAP-07 |
| [JMAP-17](JMAP-17.md) | расширения: списки отправителей и оргструктура | jmap | A | jmap-08 | todo | CTR-35,JMAP-01 |
| [JMAP-18](JMAP-18.md) | аудит доступа к ящику в JMAP | jmap | A | jmap-08 | todo | CTR-35,JMAP-05 |
| [JMAP-14](JMAP-14.md) | автонастройка клиентов | jmap | A | jmap-14 | done | JMAP-01 |
| [IMAP-15](IMAP-15.md) | аудит доступа к ящику в IMAP | imap | A | imap-06 | todo | CTR-35,IMAP-07 |
| [ADM-10](ADM-10.md) | Admin API: контакты, группы, оргструктура, отключённые ящики, аудит | admin | A | admin-05 | todo | CTR-35,ADM-07 |
| [APP-12](APP-12.md) | сборка: соответствие Exchange и doctor автонастройки | app | A | app-08 | todo | META-33,AUTH-10,MS-17,SMTP-27,JMAP-18,IMAP-15,ADM-10 |

## Ф1 · Решение 2026-11-04: внешние встречи — API занятости и публикации — этап 3

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-36](CTR-36.md) | точка расширения источников занятости, публикации календарей | contracts | B | contracts-24 | todo | CTR-35 |
| [META-34](META-34.md) | хранение публикаций календарей и шаблонов партнёров | meta | A | meta-16 | todo | CTR-36,META-33 |
| [JMAP-19](JMAP-19.md) | расширение Availability/get | jmap | A | jmap-09 | todo | CTR-36,JMAP-14 |
| [JMAP-20](JMAP-20.md) | расширение публикации календарей | jmap | A | jmap-09 | todo | CTR-36,JMAP-14 |

## Ф1 · Решение 2026-11-01: «Очистить корзину» — настройка; APPEND без правил доставки (`docs/decisions/2026-11-01-trash-purge.md`)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-26](CTR-26.md) | очистка корзины пользователем и настройка trash.purge | contracts | B | contracts-14 | done | CTR-07,CTR-18 |
| [CTR-27](CTR-27.md) | MailStore::append — письмо клиента без правил доставки | contracts | B | contracts-15 | done | CTR-20 |
| [META-29](META-29.md) | очистка корзины и скрытые письма в PostgreSQL | meta | B | meta-12 | done | CTR-26,META-10 |
| [IMAP-14](IMAP-14.md) | EXPUNGE в корзине — TrashStore::purge; APPEND — MailStore::append | imap | A | imap-05 | done | CTR-26,CTR-27,IMAP-07 |
| [JMAP-11](JMAP-11.md) | Email/set destroy в корзине — purge; Email/import — append | jmap | A | jmap-05 | done | CTR-26,CTR-27,JMAP-05 |
| [ADM-09](ADM-09.md) | настройка trash.purge и восстановление скрытых писем | admin | A | admin-04 | done | CTR-26,ADM-03 |

## Ф1 · Отчёт jmap-02 (PR #164)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-28](CTR-28.md) | Outbound в контрактах | contracts | B | contracts-16 | done | SMTP-06 |
| [CTR-29](CTR-29.md) | move_to сохраняет MessageId | contracts | B | contracts-17 | done | CTR-05 |
| [CTR-30](CTR-30.md) | хранилище Identity и EmailSubmission | contracts | B | contracts-18 | done | CTR-28 |
| [JMAP-12](JMAP-12.md) | владелец загруженного blob | jmap | A | jmap-06 | done | JMAP-06 |

## Ф1 · Отчёты admin-02 (#174) и blob-06 (#175)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [PLT-18](PLT-18.md) | служебный HTTP — только внутренние адреса | platform | A | platform-08 | done | PLT-06 |
| [CTR-31](CTR-31.md) | модерация групп рассылки — доставка одобренного письма | contracts | B | contracts-19 | done | CTR-21 |
| [CTR-32](CTR-32.md) | поиск учётной записи по external_id | contracts | B | contracts-20 | done | CTR-05 |
| [MS-15](MS-15.md) | объекты писем через BlobWriter (сжатие) | mailstore | A | mailstore-09 | done | BLOB-13 |

## Ф1 · Остаток 2026-11-01 (отчёты imap-05, mailstore-09, app-02)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-33](CTR-33.md) | перенос в корзину снимает \Deleted | contracts | B | contracts-21 | done | CTR-26 |
| [APP-10](APP-10.md) | подключение после app-02 — корзина, сжатие, нагрузка | app | A | app-06 | done | APP-04,IMAP-14,JMAP-11,MS-15 |
| [JMAP-13](JMAP-13.md) | загрузка, совпавшая со сжатой частью письма | jmap | A | jmap-07 | done | JMAP-06,MS-15 |

## Ф1 · Находки CMP-01: проверка клиентом Postman 2026-11-05 (приоритет)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CMP-01](CMP-01.md) | эталон `--profile smb` принимает соединения | deploy | A | deploy-08 | done | — |
| [CMP-02](CMP-02.md) | каталоги ролей создаются до наложения Landlock | app | A | app-09 | done | — |
| [CMP-06](CMP-06.md) | индексатор — повтор после ошибки с нарастающей паузой | search | A | search-03 | done | — |
| [CMP-03](CMP-03.md) | EmailSubmission/query и queryChanges | jmap | A | jmap-10 | done | — |
| [CMP-04](CMP-04.md) | отправка локальному адресату — доставка сразу, без внешней очереди | smtp | A | smtp-08 | done | — |
| [CMP-05](CMP-05.md) | системные папки при создании учётной записи и при первом входе | meta | A | meta-17 | done | — |
| [CMP-08](CMP-08.md) | `Outbound` JMAP со статусами по получателям | jmap | A | jmap-11 | done | CMP-03 |
| [CMP-07](CMP-07.md) | отправка из JMAP через `Delivery` — локальным адресатам сразу | app | A | app-10 | done | CMP-04,CMP-08 |
| [KIT-07](KIT-07.md) | in-memory ящик — 6 системных папок и дополнение при открытии | testkit | A | testkit-04 | done | CMP-05 |
| [META-35](META-35.md) | ошибка одного раздела журнала не прерывает опрос остальных | meta | A | meta-18 | done | — |
| [CMP-09](CMP-09.md) | метрики индексатора и doctor для служебных доменов | app | A | app-11 | done | CMP-06 |
| [APP-13](APP-13.md) | подключение расширений IMAP, POP3 и PSI в `serve` | app | A | app-12 | done | IMAP-12,PLT-17 |
| [PLT-19](PLT-19.md) | события файловой системы через `notify` | platform | A | platform-09 | done | PLT-17 |
| [SYS-08](SYS-08.md) | Landlock — корневые сертификаты по ссылкам Debian | sys | A | sys-04 | done | SYS-07 |
| [SYS-09](SYS-09.md) | ожидание событий PSI в tp-sys | sys | A | sys-04 | done | PLT-17 |
| [CTR-37](CTR-37.md) | системный сигнал изменения корзины | contracts | B | contracts-25 | done | CTR-23 |
| [IMAP-16](IMAP-16.md) | POP3 — блокировка ящика; запись в общие папки по ACL | imap | A | imap-07 | done | IMAP-12,IMAP-10 |
| [APP-14](APP-14.md) | ACME и переключение СУБД в `serve` | app | A | app-13 | done | META-25,DEP-05 |
| [CTR-38](CTR-38.md) | контракты GC, владельца загрузки и восстановления скрытых писем | contracts | B | contracts-26 | done | CTR-23 |
| [SMTP-28](SMTP-28.md) | `Delivery::submit` без очереди — ошибка только для внешних получателей | smtp | A | smtp-09 | done | CMP-04 |
| [META-36](META-36.md) | `EventBus` для `PgSignalBus`; сигнал `TrashChanged` из PostgreSQL | meta | A | meta-19 | done | META-26,CTR-37 |
| [MS-18](MS-18.md) | таймер корзины по сигналу `TrashChanged` | mailstore | A | mailstore-11 | done | MS-14,CTR-37 |
| [APP-15](APP-15.md) | сведения об узле в `heartbeat` | app | A | app-14 | done | META-27 |
| [CTR-39](CTR-39.md) | сигнал при снятии блокировки; пометка Membership | contracts | B | contracts-27 | done | CTR-23 |
| [SYS-10](SYS-10.md) | дескрипторы сокетов systemd в `TcpListener` | sys | A | sys-05 | done | SYS-01 |
| [APP-16](APP-16.md) | сокеты systemd, политики доменов в очереди, согласование doctor | app | A | app-15 | done | SYS-10,SMTP-15 |
| [CTR-40](CTR-40.md) | адресация организации в данных; перенос получателей без попытки | contracts | B | contracts-28 | done | CTR-24 |
| [PLT-20](PLT-20.md) | процессный `CryptoProvider` rustls для ACME | platform | A | platform-10 | done | PLT-11 |
| [META-38](META-38.md) | `PgMeta` поверх `Failover` — повтор операций при переключении | meta | A | meta-21 | done | META-25 |
| [META-39](META-39.md) | переписывание адресов, `postpone` и `Unblocked` в PostgreSQL | meta | A | meta-23 | done | CTR-40,CTR-39 |
| [META-40](META-40.md) | повтор записей по ключу при переключении СУБД | meta | A | meta-24 | done | META-38 |
| [SMTP-29](SMTP-29.md) | переписывание адресов из данных организации; `postpone` в планировщике | smtp | A | smtp-10 | done | CTR-40,SMTP-13,SMTP-14 |
| [CLU-12](CLU-12.md) | транспорт Rpc узел → узел (HTTPS/2, mTLS) | cluster | A | cluster-07 | done | CLU-11 |
| [CLU-13](CLU-13.md) | `LocalBus` — `Resync` при отставании подписчика | cluster | A | cluster-08 | done | CLU-09 |
| [DEP-11](DEP-11.md) | убрать Valkey из стенда разработки | deploy | A | deploy-09 | done | CLU-09 |
| [IMAP-17](IMAP-17.md) | IDLE — полная сверка по `Resync` | imap | A | imap-08 | done | CLU-13 |
| [JMAP-21](JMAP-21.md) | push — изменение всех типов по `Resync` | jmap | A | jmap-12 | done | CLU-13 |
| [SYS-11](SYS-11.md) | проба Landlock без `/sys/kernel/security` | sys | A | sys-06 | done | SYS-07 |
| [PLT-21](PLT-21.md) | doctor — проба Landlock; подпись метрики отставания шины | platform | A | platform-11 | done | SYS-11,CLU-13 |
| [APP-17](APP-17.md) | подключение журнала доставки, групп, обучения антиспама и защиты от перебора | app | A | app-16 | done | SMTP-16,SMTP-17,SMTP-18,AUTH-08,APP-16 |
| [APP-18](APP-18.md) | подключение транспорта организации и Rpc узла | app | A | app-17 | ready | SMTP-29,CLU-12,APP-15 |
| [META-41](META-41.md) | слушатель сигналов и миграции при списке адресов СУБД | meta | A | meta-25 | ready | META-38 |
| [CLU-14](CLU-14.md) | Rpc — идентичность узла по SAN, HTTP/2 | cluster | A | cluster-09 | ready | CLU-12 |
| [META-27](META-27.md) | состав кластера с ролями и блокировки | meta | A | meta-20 | done | CTR-23,META-16 |

## Ф1 · Ответы владельца 2026-11-10 (#193, #195, #205, #253, #272)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-41](CTR-41.md) | локальный токен первого администратора | contracts | B | contracts-29 | done | CTR-23 |
| [ADM-11](ADM-11.md) | `thepost admin init` — первый администратор | admin | A | admin-06 | done | CTR-41 |
| [APP-19](APP-19.md) | команда `thepost admin init` и локальный сокет | app | A | app-18 | done | ADM-11 |
| [META-43](META-43.md) | локальные токены Admin API в PostgreSQL, подключение `admin init` | meta | B | meta-27 | done | CTR-41,ADM-11,APP-19 |
| [CTR-42](CTR-42.md) | хранилище отправок JMAP (отложенная отправка и отмена) | contracts | B | contracts-30 | ready | CTR-28 |
| [META-42](META-42.md) | системные папки — только по роли, суффикс при совпадении имени | meta | A | meta-26 | done | CMP-05 |
| [KIT-08](KIT-08.md) | фейк ящика — системные папки только по роли | testkit | A | testkit-05 | done | KIT-07 |
| [AUTH-11](AUTH-11.md) | `auth.abuse.fail_closed` | auth | A | auth-06 | done | AUTH-08 |
| [DEP-09](DEP-09.md) | стенды без Valkey, эталоны HA СУБД | deploy | A | deploy-10 | done | CLU-09 |

## Ф1 · Решения 2026-11-11: шарды, токен администратора, цепочка поставки, отчёты этапа 2 (`docs/decisions/2026-11-11-contracts-ci-defaults.md`, `2026-11-11-stage2-followups.md`)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [DEPS-01](DEPS-01.md) | обновить calamine и libsqlite3-sys, сократить пропуски cargo audit | deps | B | deps-01 | done | CI-03 |
| [DEPS-02](DEPS-02.md) | mail-auth ≥ 0.14 и mail-send 0.7 — без zip 8 | deps | B | deps-02 | done | DEPS-01 |
| [DEPS-03](DEPS-03.md) | зависимость cms для подписи профиля .mobileconfig | deps | B | deps-03 | done | JMAP-14 |
| [JMAP-22](JMAP-22.md) | подпись профиля .mobileconfig сертификатом сервера | jmap | A | jmap-15 | done | JMAP-14,DEPS-03 |
| [ADM-12](ADM-12.md) | постраничный список учётных записей в Admin API | admin | A | admin-07 | done | ADM-11 |
| [APP-20](APP-20.md) | fail_closed защиты от перебора в узле, устаревший TODO doctor | app | A | app-19 | done | AUTH-11,APP-08 |
| [APP-21](APP-21.md) | CLI thepost admin на структурах tp-admin | app | A | app-20 | done | APP-11 |
| [CTR-44](CTR-44.md) | AccountStore::list — постраничный список учётных записей | contracts | B | contracts-32 | done | ADM-12 |
| [META-45](META-45.md) | побайтный порядок списка учётных записей — индекс COLLATE "C" | meta | B | meta-29 | done | CTR-44 |
| [ADM-13](ADM-13.md) | Serialize и Deserialize у структур Admin API | admin | A | admin-08 | done | ADM-12 |
| [APP-22](APP-22.md) | CLI thepost admin без копий структур | app | A | app-21 | done | ADM-13,APP-21 |
| [PLT-23](PLT-23.md) | текущая цепочка и ключ TLS для подписи | platform | A | platform-13 | done | PLT-22 |
| [APP-23](APP-23.md) | подпись профиля .mobileconfig в узле | app | A | app-22 | done | PLT-23,JMAP-22 |
| [DEP-12](DEP-12.md) | стенд SMB — учётные записи через thepost admin и наполнение по Д.3 | deploy | A | deploy-12 | ready | DEP-04,APP-21 |
| [DEP-13](DEP-13.md) | стенд SMB — замеры и отчёт по критериям Д.7 | deploy | A | deploy-12 | ready | DEP-12 |
| [DEP-14](DEP-14.md) | стенд SMB — стресс, ×3, отказы и длительный прогон | deploy | A | deploy-12 | ready | DEP-13 |
| [DEP-15](DEP-15.md) | резервное копирование SMB — pgBackRest и копия каталога писем | deploy | A | deploy-13 | ready | DEP-05 |
| [DOC-01](DOC-01.md) | docs/PREREQUISITES.md — требования и тонкости настройки окружения | docs | A | docs-01 | done | — |
| [APP-24](APP-24.md) | thepost doctor — кодировка и LC_CTYPE базы | app | A | app-23 | done | — |
| [DEP-16](DEP-16.md) | эталоны задают кодировку UTF8 и LC_CTYPE UTF-8 базы | deploy | A | deploy-14 | ready | — |
| [DEP-17](DEP-17.md) | эталон — UTC в PostgreSQL, chrony, текст ошибки «база не в UTF-8» | deploy | A | deploy-15 | todo | DEP-16 |
| [DEP-18](DEP-18.md) | thepost_pki — перевыпуск внутреннего сертификата до конца срока | deploy | A | deploy-15 | todo | — |
| [APP-25](APP-25.md) | thepost doctor — срок внутренних сертификатов | app | A | app-24 | ready | — |
| [SYS-12](SYS-12.md) | Landlock — резолвер systemd-resolved (Ubuntu 24.04) | sys | A | sys-07 | ready | — |
| [DEP-24](DEP-24.md) | эталон на Debian 13 и Ubuntu 26.04 | deploy | A | deploy-16 | todo | DEP-17 |
| [DEP-19](DEP-19.md) | эталон Ansible на Astra Linux SE 1.8 | deploy | A | deploy-16 | todo | DEP-15,DEP-17 |
| [DEP-20](DEP-20.md) | эталон Ansible на РЕД ОС 8 | deploy | A | deploy-16 | todo | DEP-15,DEP-17 |
| [APP-30](APP-30.md) | cluster.listen — адрес приёма межузловых вызовов отдельно от объявляемого | app | A | app-29 | todo | APP-18 |
| [DEP-23](DEP-23.md) | резервная копия OpenBao — снимок Raft | deploy | A | deploy-19 | todo | DEP-15 |
| [PLT-22](PLT-22.md) | необязательный сертификат клиента на слушателях SMTP | platform | A | platform-12 | done | SMTP-20 |
| [CTR-43](CTR-43.md) | очередь модерации — и для правил потока почты | contracts | B | contracts-31 | ready | CTR-31,SMTP-22 |
| [META-44](META-44.md) | очередь модерации без группы в PostgreSQL | meta | B | meta-28 | todo | CTR-43 |
| [SMTP-30](SMTP-30.md) | исход Moderate правил потока — в очередь модерации | smtp | A | smtp-11 | todo | CTR-43,SMTP-22 |
| [CTR-49](CTR-49.md) | учётная запись в очереди и учёте BLOB, сигнал ShardsChanged | contracts | B | contracts-36 | todo | CTR-35 |
| [CTR-50](CTR-50.md) | подключение отключённого ящика — опция «оставить» / «удалить» содержимое | contracts | B | contracts-36 | todo | CTR-35 |
| [DEPS-06](DEPS-06.md) | HTTP/2 для межузловых вызовов — фичи axum и reqwest | deps | B | deps-06 | todo | CLU-14 |
| [CLU-15](CLU-15.md) | Rpc — h2 в ALPN | cluster | A | cluster-10 | todo | DEPS-06 |
| [DEP-22](DEP-22.md) | сертификаты узлов с URI thepost:node:<id> | deploy | A | deploy-20 | todo | CLU-14 |
| [PLT-24](PLT-24.md) | ключи TLS из OpenBao | platform | A | platform-14 | ready | PLT-23 |

## Ф2. Совместная работа (≈5 мес.)

| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |
|---|---|---|---|---|---|---|
| [CTR-45](CTR-45.md) | календари и контакты — коллекции, объекты, журнал изменений | contracts | B | contracts-33 | ready | CTR-03,CTR-04 |
| [CTR-46](CTR-46.md) | Sieve — скрипты, автоответ, точка правил доставки | contracts | B | contracts-33 | ready | CTR-05,CTR-10 |
| [DEPS-04](DEPS-04.md) | зависимости крейтов Ф2 — tp-dav, tp-sieve, tp-jmap | deps | B | deps-04 | todo | CTR-45,CTR-46 |
| [META-46](META-46.md) | схема календарей и контактов | meta | B | meta-30 | todo | CTR-45 |
| [META-47](META-47.md) | DavStore на PostgreSQL | meta | A | meta-30 | todo | META-46 |
| [META-48](META-48.md) | журнал изменений DAV — токены синхронизации и надгробия | meta | A | meta-30 | todo | META-47 |
| [META-49](META-49.md) | хранение скриптов Sieve и автоответа | meta | B | meta-31 | todo | CTR-46 |
| [META-50](META-50.md) | журнал автоответов | meta | A | meta-31 | todo | META-49 |
| [DAV-01](DAV-01.md) | HTTP-каркас DAV и вход | dav | A | dav-01 | todo | DEPS-04,CTR-45 |
| [DAV-02](DAV-02.md) | PROPFIND, PROPPATCH, OPTIONS и XML multistatus | dav | A | dav-01 | todo | DAV-01 |
| [DAV-03](DAV-03.md) | ресурсы — GET, PUT, DELETE, ETag, MKCOL и MKCALENDAR | dav | A | dav-01 | todo | DAV-02 |
| [DAV-04](DAV-04.md) | синхронизация — sync-collection и getctag | dav | A | dav-01 | todo | DAV-03 |
| [DAV-05](DAV-05.md) | CalDAV — дом календарей и свойства | dav | A | dav-02 | todo | DAV-04 |
| [DAV-06](DAV-06.md) | CalDAV — calendar-query и free-busy-query | dav | A | dav-02 | todo | DAV-05 |
| [DAV-07](DAV-07.md) | CalDAV — calendar-multiget и задачи VTODO | dav | A | dav-02 | todo | DAV-05 |
| [DAV-08](DAV-08.md) | CalDAV — проверка объектов и ручная проверка клиентами | dav | A | dav-02 | todo | DAV-06,DAV-07 |
| [DAV-09](DAV-09.md) | CardDAV — дом адресных книг и свойства | dav | A | dav-03 | todo | DAV-04 |
| [DAV-10](DAV-10.md) | CardDAV — addressbook-query и addressbook-multiget | dav | A | dav-03 | todo | DAV-09 |
| [DAV-11](DAV-11.md) | CardDAV — версии vCard и проверка карточек | dav | A | dav-03 | todo | DAV-10 |
| [DAV-12](DAV-12.md) | глобальная адресная книга (GAL) только для чтения | dav | A | dav-03 | todo | DAV-09 |
| [SIEVE-01](SIEVE-01.md) | разбор скриптов Sieve — лексер и грамматика | sieve | A | sieve-01 | todo | DEPS-04 |
| [SIEVE-02](SIEVE-02.md) | проверка скрипта — require, аргументы, сообщения об ошибках | sieve | A | sieve-01 | todo | SIEVE-01 |
| [SIEVE-03](SIEVE-03.md) | интерпретатор — управление, тесты ядра, сравнения | sieve | A | sieve-01 | todo | SIEVE-02 |
| [SIEVE-04](SIEVE-04.md) | действия — fileinto, copy, imap4flags, redirect, mailbox | sieve | A | sieve-02 | todo | SIEVE-03 |
| [SIEVE-05](SIEVE-05.md) | тесты envelope, body, relational, subaddress | sieve | A | sieve-02 | todo | SIEVE-03 |
| [SIEVE-06](SIEVE-06.md) | автоответ vacation | sieve | A | sieve-02 | todo | SIEVE-03 |
| [SIEVE-07](SIEVE-07.md) | правила доставки — реализация DeliveryRules и фаззинг | sieve | A | sieve-02 | todo | SIEVE-04,SIEVE-05,SIEVE-06 |
| [SMTP-31](SMTP-31.md) | правила Sieve при локальной доставке | smtp | A | smtp-12 | todo | CTR-46,SMTP-28 |
| [SMTP-32](SMTP-32.md) | отправка автоответа | smtp | A | smtp-12 | todo | SMTP-31 |
| [JMAP-23](JMAP-23.md) | VacationResponse — автоответ из JMAP | jmap | A | jmap-16 | todo | CTR-46,JMAP-02 |
| [JMAP-24](JMAP-24.md) | JMAP Sieve — SieveScript (RFC 9661) | jmap | A | jmap-16 | todo | CTR-46,SIEVE-02,JMAP-02 |
| [JMAP-25](JMAP-25.md) | JMAP Contacts — AddressBook (RFC 9610) | jmap | A | jmap-16 | todo | CTR-45,JMAP-02 |
| [JMAP-26](JMAP-26.md) | JMAP Contacts — ContactCard | jmap | A | jmap-16 | todo | JMAP-25,DAV-03 |
| [ADM-14](ADM-14.md) | Admin API — правила Sieve и автоответ | admin | A | admin-09 | todo | CTR-46,SIEVE-02 |
| [APP-29](APP-29.md) | календари и контакты в узле — роль dav | app | A | app-28 | todo | DAV-08,DAV-11,DAV-12,META-48 |
| [APP-26](APP-26.md) | правила Sieve и автоответ в узле — доставка, Admin API, JMAP VacationResponse | app | A | app-25 | todo | SIEVE-07,SMTP-32,META-50,ADM-14,JMAP-23 |
| [DEP-21](DEP-21.md) | стенд — CalDAV, CardDAV, ManageSieve | deploy | A | deploy-17 | todo | APP-29 |
| [SIEVE-08](SIEVE-08.md) | ManageSieve — протокол и вход | sieve | A | sieve-03 | todo | SIEVE-02 |
| [SIEVE-09](SIEVE-09.md) | ManageSieve — команды скриптов | sieve | A | sieve-03 | todo | SIEVE-08 |
| [JMAP-27](JMAP-27.md) | JMAP Calendars — Calendar | jmap | A | jmap-17 | todo | CTR-45,DAV-03,JMAP-25 |
| [JMAP-28](JMAP-28.md) | JMAP Calendars — CalendarEvent | jmap | A | jmap-17 | todo | JMAP-27 |
| [JMAP-29](JMAP-29.md) | JMAP Calendars — CalendarEvent/query и развёртывание повторов | jmap | A | jmap-17 | todo | JMAP-28 |
| [APP-27](APP-27.md) | ManageSieve и JMAP календари, контакты, Sieve в узле | app | A | app-26 | todo | SIEVE-09,JMAP-29,APP-29,APP-26 |
| [CI-06](CI-06.md) | полоса console и сборка веб-консоли в CI | ci | B | ci-05 | ready | — |
| [DEPS-05](DEPS-05.md) | rust-embed — веб-консоль внутри бинарного файла | deps | B | deps-05 | todo | CI-06 |
| [UI-01](UI-01.md) | каркас веб-консоли на Refine | console | A | console-01 | todo | CI-06 |
| [UI-02](UI-02.md) | вход, провайдер данных, типы из OpenAPI, покрытие API | console | A | console-01 | todo | UI-01 |
| [UI-03](UI-03.md) | организации, домены и политики | console | A | console-01 | todo | UI-02 |
| [UI-04](UI-04.md) | учётные записи и пароли приложений | console | A | console-02 | todo | UI-03 |
| [UI-05](UI-05.md) | группы рассылки и модерация | console | A | console-02 | todo | UI-03 |
| [UI-06](UI-06.md) | корзина учётной записи | console | A | console-02 | todo | UI-04 |
| [UI-07](UI-07.md) | транспорт — принятые домены, источники, маршруты | console | A | console-03 | todo | UI-03 |
| [UI-08](UI-08.md) | журнал доставки и журнал аудита | console | A | console-03 | todo | UI-03 |
| [UI-09](UI-09.md) | правила и автоответ пользователя; покрытие Admin API | console | A | console-03 | todo | UI-04,UI-05,UI-06,UI-07,UI-08,ADM-14 |
| [ADM-15](ADM-15.md) | раздача веб-консоли ролью admin | admin | A | admin-10 | todo | DEPS-05,UI-01 |
| [CTR-47](CTR-47.md) | планирование встреч — iTIP и занятость | contracts | B | contracts-34 | todo | CTR-45 |
| [META-51](META-51.md) | входящие планирования и индекс занятости | meta | B | meta-32 | todo | CTR-47,META-47 |
| [DAV-13](DAV-13.md) | планирование внутри организации — RFC 6638 | dav | A | dav-04 | todo | DAV-08,CTR-47 |
| [DAV-14](DAV-14.md) | iMIP наружу — приглашения внешним участникам | dav | A | dav-04 | todo | DAV-13 |
| [DAV-15](DAV-15.md) | приём iTIP — обработка приглашений и ответов | dav | A | dav-04 | todo | DAV-13 |
| [DAV-16](DAV-16.md) | занятость — VFREEBUSY и реализация FreeBusy | dav | A | dav-04 | todo | DAV-13 |
| [SMTP-33](SMTP-33.md) | приём iMIP — приглашения и ответы извне | smtp | A | smtp-13 | todo | CTR-47,SMTP-31 |
| [JMAP-30](JMAP-30.md) | JMAP — Principal и занятость | jmap | A | jmap-18 | todo | JMAP-29,CTR-47 |
| [JMAP-31](JMAP-31.md) | JMAP — приглашения на встречи | jmap | A | jmap-18 | todo | JMAP-30,DAV-13 |
| [APP-28](APP-28.md) | планирование встреч в узле | app | A | app-27 | todo | DAV-16,SMTP-33,META-51,JMAP-31,APP-27 |
| [CMP-10](CMP-10.md) | приёмка Ф2 — календари, контакты, правила, консоль | deploy | A | deploy-18 | todo | APP-28,UI-09,DEP-21 |
| [CTR-48](CTR-48.md) | общий доступ к календарям и контактам, представители, ресурсы | contracts | B | contracts-35 | todo | CTR-45,CTR-47 |
| [META-52](META-52.md) | права на коллекции, представители, правила ресурсов | meta | B | meta-33 | todo | CTR-48 |
| [AUTH-12](AUTH-12.md) | Authorizer — права на календари и адресные книги | auth | A | auth-07 | todo | CTR-48,AUTH-07 |
| [DAV-17](DAV-17.md) | общий доступ к календарям и адресным книгам | dav | A | dav-05 | todo | DAV-16,CTR-48,AUTH-12 |
| [DAV-18](DAV-18.md) | представители — calendar-proxy | dav | A | dav-05 | todo | DAV-17 |
| [DAV-19](DAV-19.md) | ресурсы — бронирование переговорных | dav | A | dav-05 | todo | DAV-17 |
| [JMAP-32](JMAP-32.md) | JMAP — общий доступ к календарям и адресным книгам | jmap | A | jmap-19 | todo | JMAP-31,CTR-48 |
| [JMAP-33](JMAP-33.md) | JMAP Tasks поверх VTODO | jmap | A | jmap-19 | todo | JMAP-29 |
| [ADM-16](ADM-16.md) | Admin API — ресурсы, представители, общий доступ | admin | A | admin-11 | todo | CTR-48 |
| [UI-10](UI-10.md) | ресурсы, представители, общий доступ | console | A | console-04 | todo | UI-09,ADM-16 |
| [UI-11](UI-11.md) | массовые операции — импорт учётных записей из CSV | console | A | console-04 | todo | UI-09 |
| [DAV-20](DAV-20.md) | публикация календаря по ссылке ICS | dav | A | dav-06 | todo | DAV-17,CTR-36,META-34 |
| [DAV-21](DAV-21.md) | ссылка занятости FBURL | dav | A | dav-06 | todo | DAV-20,DAV-16 |
| [DAV-22](DAV-22.md) | оргструктура в глобальной адресной книге | dav | A | dav-06 | todo | DAV-12,CTR-35,AUTH-10 |

## Вне волн

| Задача | Суть | Полоса | Класс | Зависит |
|---|---|---|---|---|
| [AUTH-09](AUTH-09.md) | пароли приложений на контракте AppPasswordStore | auth | A | CTR-22 |
| [CTR-22](CTR-22.md) | контракт AppPasswordStore (пароли приложений) | contracts | B | CTR-19,AUTH-02 |
| [CTR-25](CTR-25.md) | контракты Admin API — политики уровней, домены, журнал аудита | contracts | B | ADM-02,CTR-24,META-22 |
