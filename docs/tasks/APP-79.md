# APP-79: узел — источники TI: коннекторы «ключ заказчика», импорт txt/CSV/RPZ/MISP/STIX, TAXII 2.1, TTL

lane: app
deps: [APP-78]
class: A
files: [crates/thepost/src/*, crates/thepost/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/CTR-95.md, docs/tasks/SMTP-52.md, docs/tasks/SIEVE-15.md]
checks: [nextest]
manual: no

## Сделать
- Списки TI в памяти узла: по источнику — текущая версия из BLOB (`list_current`), индексы — префиксы IP (`ipnet`),
  домены (с поддоменами), URL (нормализованные), SHA-256; интерфейс поиска — SMTP (`list_match`, SMTP-52) и Sieve
  (`extlists`, SIEVE-15). Учёт обращений и совпадений по организации — счётчики в памяти, сброс в `record_usage`
  раз в минуту.
- Уровень 1 (поставка): списки пакета поставщика (Feodo Tracker, SSLBL, Spamhaus DROP/EDROP) — из APP-78, источник
  `Bundled` с атрибуцией в `doctor` и отчёте пакета.
- Уровень 2 (ключ заказчика, мы не раздаём): `kind: Http` с ключом из хранилища секретов по `SecretRef` — abuse.ch
  URLhaus, MalwareBazaar, ThreatFox (заголовок `Auth-Key`), коммерческий OpenPhish; Google Web Risk — Update API
  (локальная база префиксов хэшей, подтверждение совпадения онлайн `hashes.search`; без сети признак не
  срабатывает, предупреждение). DNSBL (Spamhaus DQS, SURBL, URIBL), SaneSecurity и ClamAV — не здесь: через Rspamd и
  `clamd` (DEP-57). Пресетов запрещённых фидов (решение 126 ж) нет.
- Российские фиды (решение 126 з) — пресеты уровня 2 поверх тех же `Taxii` и `Http`, без своего парсера на вендора:
  `kaspersky_tdf`: TAXII 2.1 `https://taxii.tip.kaspersky.com/v2`, `HttpAuth::Basic { user: "taxii" }` + токен; либо
  HTTPS-фид с `ClientCert`. Далее `bizone_ti` (TAXII 2.1), `f6_ti` (TAXII или REST), `pt_feeds` (REST либо файл
  STIX/CSV из PT Fusion в импорт), `solar_ti` (только импорт). Адрес TAXII и коллекции вводит администратор из
  кабинета вендора: публично их нет. Пресет заполняет `license` (ссылка на условия, `commercial: false` —
  внутреннее использование) и предупреждает в `doctor`, если источник включён для `tenants: All` в установке MSSP.
- RST Cloud (решение 126 и) — пресет `rst_cloud`: `Http { url: https://api.rstcloud.net/v1/{ip|domain|url|hash}?type=stix&date=latest }`,
  `HttpAuth::Header { name: "X-Api-Key" }`, ответ gzip, формат `Stix21`; раз в сутки. Один пресет обслуживает и ключ
  заказчика, и наш ключ. Наш ключ — только у организаций стенда (`tenants: Only`, `tier: CustomerKey`), в пакет
  поставщика не попадает; `Bundled` с RST отклоняется (`FORBIDDEN_BUNDLED`), пока владелец после договора не решит
  иначе. Noise Control и Report Hub — не в этой карточке.
- Маркировка TLP: STIX `object_marking_refs` (TLP 2.0) и теги MISP `tlp:*` — в версию списка; показывается в `doctor`,
  API и отчёте о совпадении; уровень `red` при импорте — предупреждение. Значения из списков с TLP не пишутся в
  журнал и метрики: только идентификатор источника и счётчики.
- Уровень 3 (импорт): форматы `Txt` (строка — значение, `#` — комментарий), `Csv { column }`, `Rpz` (зона RPZ: имена
  — домены, `rpz-ip` — префиксы), `MispFeed` (`manifest.json` и события JSON: атрибуты `ip-dst`, `domain`, `url`,
  `sha256`), `Stix21` — объекты `indicator` с `pattern_type: stix`; из языка шаблонов только равенства
  `ipv4-addr:value`, `ipv6-addr:value`, `domain-name:value`, `url:value`, `file:hashes.'SHA-256'` (`winnow`),
  остальное пропускается со счётчиком; `valid_until` и `revoked` учитываются. Клиент TAXII 2.1:
  `collections/{id}/objects/` с `added_after` и постраничностью (`next`), авторизация — `SecretRef` с `HttpAuth`.
  Импорт файла — маршрут ADM-51 и каталог `risk.import.dir`.
- ГосСОПКА и ФинЦЕРТ (решение 126 к) — только импорт, прямого клиента к НКЦКИ и АСОИ нет. Источник с
  `origin: GosSopka|FinCert` приходит из TIP заказчика (R-Vision TIP, Security Vision TIP, Kaspersky CyberTrace) по
  TAXII 2.1 / STIX 2.1 или фидом MISP, либо файлом txt, CSV или STIX по расписанию в `risk.import.dir`. Он всегда
  привязан к организациям (`tenants: Only`) и не уходит в пакеты и выгрузки за пределы установки.
- Загрузка источника — роль maintenance раз в его период: данные — в BLOB вне транзакции, затем `put_list_version`;
  пределы размера и числа записей — `risk.sources.max_bytes`, `risk.sources.max_entries`; при ошибке остаётся прежняя
  версия.
- TTL: версия старше `ttl` — источник устарел: признаки по `StaleAction` (по умолчанию вес −50 %), событие аудита,
  предупреждение в `thepost doctor` и метрика; молча не отключается.
- MSSP: источник с `tenants: Only(…)` ищется только для этих организаций; учёт по организации — для договора MSSP.

## Как делают другие
- Rspamd `multimap` с картами по HTTP (`If-Modified-Since`) и `fallback+file://`
  (https://docs.rspamd.com/modules/multimap); MISP feeds (https://www.misp-project.org/feeds/); TAXII 2.1 и STIX 2.1
  (https://docs.oasis-open.org/cti/taxii/v2.1/, https://docs.oasis-open.org/cti/stix/v2.1/); RPZ — BIND и Unbound;
  abuse.ch API с `Auth-Key` (https://urlhaus.abuse.ch/api/); Web Risk Update API
  (https://docs.cloud.google.com/web-risk/docs/overview); коннектор Kaspersky Feeds для OpenCTI — TAXII 2.1, Basic
  `taxii` + токен (https://github.com/KasperskyLab/threat-intelligence); клиент RST `rstapi-python` — `X-Api-Key`,
  `/v1/{тип}?type=&date=latest` (https://github.com/rstcloud/rstapi-python); TIP R-Vision и Security Vision уже
  принимают НКЦКИ и ФинЦЕРТ (исследование, раздел ж).
- Отличие: у каждого источника TTL и действие при устаревании; MISP — только формат, код MISP (AGPL) не встраиваем.

## Приёмка
- Тесты разбора: txt с комментариями, CSV по столбцу, RPZ с `rpz-ip`, фид MISP, пакет STIX с `revoked` и
  неподдерживаемым шаблоном — значения и счётчик пропусков.
- e2e: TAXII-сервер на тестовом HTTP отдаёт две страницы — обе загружены, повтор с `added_after` — без дублей;
  письмо со ссылкой из импортированного списка — признак `list_match`; источник старше TTL — вес снижен, в `doctor`
  предупреждение; источник MSSP для организации A не срабатывает у B; учёт по организациям в `usage`.
- Пресеты: TAXII с `Basic { user: "taxii" }` — на тестовом сервере проверяется заголовок `Authorization`; `rst_cloud`
  на тестовом HTTP — проверяется заголовок `X-Api-Key`, gzip распакован, STIX разобран. Пакет поставщика с
  источником `rst_cloud` — lint отклоняет.
- Импорт STIX с `object_marking_refs` TLP:AMBER и `origin: GosSopka` — TLP в версии списка; без `tenants: Only`
  создать нельзя; значение индикатора не попадает в журнал.
