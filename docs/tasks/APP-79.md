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
- Уровень 3 (импорт): форматы `Txt` (строка — значение, `#` — комментарий), `Csv { column }`, `Rpz` (зона RPZ: имена
  — домены, `rpz-ip` — префиксы), `MispFeed` (`manifest.json` и события JSON: атрибуты `ip-dst`, `domain`, `url`,
  `sha256`), `Stix21` — объекты `indicator` с `pattern_type: stix`; из языка шаблонов только равенства
  `ipv4-addr:value`, `ipv6-addr:value`, `domain-name:value`, `url:value`, `file:hashes.'SHA-256'` (`winnow`),
  остальное пропускается со счётчиком; `valid_until` и `revoked` учитываются. Клиент TAXII 2.1:
  `collections/{id}/objects/` с `added_after` и постраничностью (`next`), авторизация — `SecretRef`. Импорт файла —
  маршрут ADM-51 и каталог `risk.import.dir`.
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
  (https://docs.cloud.google.com/web-risk/docs/overview).
- Отличие: у каждого источника TTL и действие при устаревании; MISP — только формат, код MISP (AGPL) не встраиваем.

## Приёмка
- Тесты разбора: txt с комментариями, CSV по столбцу, RPZ с `rpz-ip`, фид MISP, пакет STIX с `revoked` и
  неподдерживаемым шаблоном — значения и счётчик пропусков.
- e2e: TAXII-сервер на тестовом HTTP отдаёт две страницы — обе загружены, повтор с `added_after` — без дублей;
  письмо со ссылкой из импортированного списка — признак `list_match`; источник старше TTL — вес снижен, в `doctor`
  предупреждение; источник MSSP для организации A не срабатывает у B; учёт по организациям в `usage`.
