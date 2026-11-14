# DOC-14: ADMIN-GUIDE — раздел «Признаки риска и threat intelligence» с таблицей лицензий фидов

lane: docs
deps: [APP-79, DEP-57]
class: A
files: [docs/ADMIN-GUIDE.md]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/ADM-51.md, docs/tasks/APP-78.md, docs/tasks/APP-79.md, docs/tasks/DEP-57.md]
checks: []
manual: no

## Сделать
- Раздел «Признаки риска и threat intelligence» в `docs/ADMIN-GUIDE.md` по коду (и строка оглавления): что такое
  признак (детектор, вес, порог, действие, режим «только отчёт»), как считается балл и какое действие побеждает;
  уровни установка → организация, «организация — только строже»; пять стартовых признаков и связь с признаком
  ответа (раздел «Ответы на наши письма и беседы», DOC-13).
- Админский Sieve: `signal`, `environment "vnd.thepost.*"`, `extlists :list`, `:regex` — с примерами и пределами веса.
- Пакеты: подпись, lint и пробный прогон, отчёт «что изменится», автоматическая активация, откат, импорт из файла в
  закрытом контуре; команды `thepost admin risk package …`, `thepost risk verify`.
- Источники TI в три уровня, TTL и устаревание, учёт MSSP; команды `thepost admin risk source …` и маршруты.
- Таблица лицензий фидов: фид — уровень (поставка, ключ заказчика, импорт, запрещено, только с письменным
  разрешением) — лицензия или условия — атрибуция — ссылка на условия; строки по решению 126 е–ж и исследованию
  (Feodo Tracker, SSLBL, Spamhaus DROP/EDROP, PSL; Spamhaus DQS, SURBL, URIBL, URLhaus, MalwareBazaar, ThreatFox,
  OpenPhish коммерческий, Web Risk, SaneSecurity, ClamAV; OpenPhish Community, Safe Browsing, OTX, бесплатные
  ZEN/DBL/ZRD; CIRCL, PhishTank) и абзац «MSSP: договор с поставщиком фида — у MSSP».
- Российские фиды в той же таблице (решение 126 з–и), уровень — ключ заказчика: Kaspersky Threat Data Feeds
  (T&C §2.1 и §3.4 — только внутреннее использование), BI.ZONE Threat Intelligence, F6 Threat Intelligence,
  PT Threat Intelligence Feeds, Solar TI Feeds (через импорт). RST Cloud — ключ заказчика; наш ключ — только стенды
  и демонстрации, «в поставке» — только если это разрешит договор. Строка RST в таблице меняется вместе с
  `FORBIDDEN_BUNDLED`. Для каждого фида: пресет (`kaspersky_tdf`, `bizone_ti`, `f6_ti`, `pt_feeds`, `solar_ti`,
  `rst_cloud`) и что ввести из кабинета вендора (адрес TAXII, коллекция, токен или сертификат).
- Подраздел «ГосСОПКА и ФинЦЕРТ» (решение 126 к): прямого подключения узла к НКЦКИ и АСОИ ФинЦЕРТ нет. Субъект
  получает индикаторы в свою TIP (R-Vision TIP, Security Vision TIP, Kaspersky CyberTrace) и отдаёт их узлу по
  TAXII 2.1 / STIX 2.1, фидом MISP или файлом в `risk.import.dir`. Пошагово: источник `Import` с `origin` и
  `tenants: Only`, проверка TLP, что видно в `doctor`. Абзац: данные ГосСОПКА и ФинЦЕРТ не покидают установку и не
  попадают в пакеты.
- Подключение DQS, SURBL, URIBL, частного зеркала ClamAV и SaneSecurity — переменные ролей `rspamd` и `clamav_milter`.

## Как делают другие
- Справка по символам и картам Rspamd (https://docs.rspamd.com/modules/multimap) и по каналам sa-update
  (https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html); Spamhaus требует атрибуцию DROP в продукте
  (https://www.spamhaus.org/blocklists/do-not-route-or-peer/); условия Kaspersky Threat Data Feeds
  (https://media.kaspersky.com/en/Terms_and_Conditions_Threat_Data_Feeds.pdf) и RST Cloud
  (https://www.rstcloud.com/terms, https://www.rstcloud.com/eula).

## Приёмка
- Команды, маршруты, ключи конфигурации (`risk.*`, `smtp.risk`, `sieve.risk`, `sieve.regex`) и переменные ролей
  раздела совпадают с кодом и `deploy/` (`thepost admin risk --help`, `openapi_spec()`).
- Таблица лицензий совпадает с `deploy/risk/sources.toml` и решением 126.
