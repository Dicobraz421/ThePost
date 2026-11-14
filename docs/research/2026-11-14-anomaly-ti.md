# Справочник аномалий и бесплатный TI
Исследование 14.11.2026 для решения 126. Владелец выбрал: каталог признаков + Sieve vnd.thepost.risk; TI в три уровня.

**Главное.**
- Ни один готовый формат не даёт сразу три вещи: «признак + вес», безопасное расширение администратором и подписанные обновления. Ближе всего модели Rspamd (символы, баллы, карты с подписью) и Stalwart (вендорский набор правил плюс локальные правила, которые обновление не трогает).
- Стандартный Sieve не умеет считать: в нём нет арифметики и весов. Нужно своё расширение.
- По TI картина хуже, чем принято считать. Большинство «бесплатных» фидов бесплатны только для некоммерческого использования. Явно разрешены в коммерческом продукте лишь Feodo Tracker, SSLBL, Spamhaus DROP, PhishTank и SaneSecurity (у SaneSecurity с оговорками).
- Российские фиды (Kaspersky, PT, BI.ZONE, F6, Солар, RST Cloud) по публичным условиям — только для внутреннего использования, либо условия не опубликованы. Раздавать их без OEM-договора нельзя. НКЦКИ и ФинЦЕРТ доступны только самому субъекту: к нам их данные приходят импортом из TIP заказчика или файлом. Подробно — раздел (ж).

## (а) Форматы справочника

| Формат | Вес/балл | Расширяет админ | Безопасность правила | Обновление, подпись, откат |
|---|---|---|---|---|
| Rspamd (символы, multimap, phishing, Lua) | да: балл символа, вес записи карты × score | карты (ip/from/header/url/filename/content/selector), regexp, правила на Lua | regexp только совместимые с Hyperscan; Lua — произвольный код | карты по HTTP(S) с If-Modified-Since/ETag; подпись EdDSA (`sign+`, `trusted_keys`, `rspamadm signtool`); `fallback+file://`; CDB подписать нельзя. Что происходит при битой подписи — н/п |
| SpamAssassin + sa-update | да: score правила | local.cf, свои каналы | Perl-regex, возможен ReDoS | каналы подписаны GPG по умолчанию (`--nogpg` «very unsafe»); версия через DNS TXT; перед установкой обязательный lint; офлайн через `--install` из архива с .asc |
| Sieve (RFC 5228 + 5235/6134/5703/5173/5183) | **нет**: spamtest/virustest только читают готовый балл (0–10; 0–100 со `:percent`; virustest 0–5); арифметики нет | да, это наш скрипт этапа before | язык без циклов (кроме foreverypart), regex зависит от реализации | своего механизма распространения нет |
| Sigma (YAML) | нет, только level | да | — | нотация для событий журналов SIEM, о MIME не знает; правила SigmaHQ под DRL 1.1: продавать можно, но автор правила обязан попадать в вывод срабатывания |
| YARA / YARA-X | нет (метаданные) | да | YARA-X на Rust, BSD-3 | файлы правил; подписи в самом формате нет |
| Exchange: правила почтового потока | косвенно: ставит SCL −1…9 | условия и действия: пароль на вложении (Office, zip, 7z, pdf), «executable content», расширение, вложения можно проверять с учётом вложенности, regex по заголовкам | regex без учёта регистра; в содержимом вложения смотрятся первые 150 КБ | хранятся в конфигурации тенанта, подписи и отката нет |
| Defender XDR: пользовательские детекты | severity | KQL по `Email*`, частота от NRT до 24 ч; действия: перенести в Junk/Inbox, мягкое или полное удаление | до 150 оповещений за запуск | после доставки, не на SMTP |
| Stalwart (Rust, конкурент) | тег → балл задаётся в SpamTag | правило = выражение, возвращает имя тега; Sieve с `vnd.stalwart.expressions` (`eval`, `let`) — только в доверенном интерпретаторе | — | около 70 правил `STWT_` скачиваются при старте; обновление заменяет их, сохраняя флаг enable и баллы тегов; локальные правила не трогает. Подписаны ли — н/п |

Как выразить «признак с весом» в Sieve и чего не хватает:
- **Что уже есть.**
  - `foreverypart` и тесты с `:mime` (`:type`, `:subtype`, `:param "filename"`) перебирают вложения.
  - `body` (RFC 5173) проверяет текст письма.
  - `extlists` (`:list "tag:..."`) сверяет значения с внешними списками — так можно смотреть в TI.
  - `environment` (RFC 5183) позволяет регистрировать свои пункты — так можно отдавать в Sieve готовые факты.
- **Чего не хватает:** накопления балла, весов, признака «зашифрованный архив», «новый домен», «беседа доверенная».
- **Предложение:** расширение `vnd.thepost.risk` с командой `signal "<id>" [:weight N]`.
  - Вес только неотрицательный и ограничен сверху, вычесть балл нельзя. Это ровно наше «скрипт before может только сужать».
  - Факты считает Rust и отдаёт через `environment "vnd.thepost.thread" "trusted"`, а TI — через `:list`.
- **ReDoS.** Крейт Rust `regex` гарантирует поиск за O(m·n) и рассчитан на недоверенный текст. Размер шаблона ограничивается через `size_limit`. Оговорка: у `find_iter` худший случай O(m·n²).

## (б) Варианты архитектуры

1. **Каталог признаков в типизированной политике плюс админский Sieve с `vnd.thepost.risk`.**
   - Признак: id, детектор на Rust, вес, порог, действие.
   - Вендорский каталог приходит подписанным пакетом, его стоит обновлять по модели Stalwart.
   - Плюсы: одна нотация, которая у нас уже есть; монотонность по построению; детекторы (пароль на архиве, ISO/LNK, новый домен, смена Reply-To) — проверенный Rust-код; работает офлайн.
   - Минусы: новый *тип* детектора требует релиза; Sieve-расширение придётся документировать и тестировать.
2. **Свой YAML или Sigma-подобный каталог с подписанными обновлениями.**
   - Плюсы: привычно аналитикам SOC, удобно делать diff.
   - Минусы: второй язык рядом с Sieve, а это параллельный путь, против наших правил. Sigma не описывает MIME. Свой DSL — это свой парсер, свой валидатор и своя безопасность.
3. **Rspamd как движок признаков.**
   - Плюсы: зрелые multimap, phishing и подпись карт; Apache 2.0.
   - Минусы: внешний процесс на C/Lua, который надо поставлять и поддерживать в закрытом контуре. Lua-правила — это произвольный код, «только сужать» не гарантировать. Два конвейера оценки, а наш контекст доверенной беседы придётся передавать заголовками.

**Рекомендация: вариант 1 (выбран владельцем).** Он единственный сохраняет инвариант «before только сужает» на уровне языка и не добавляет второй нотации или внешнего движка. Из чужих решений стоит взять:
- из Rspamd: модель «символ + балл + карта», подпись Ed25519, `fallback` на локальный файл;
- из sa-update: обязательный lint перед установкой пакета и офлайн-установку из файла;
- из Stalwart: обновление заменяет только вендорские правила, а изменения весов администратором сохраняются;
- режим «только отчёт» для новых признаков — аналог ClassifySpam в Stalwart.

## (в) Бесплатные фиды

| Фид | Что даёт, формат, частота | Коммерция / MSSP / раздача в контур |
|---|---|---|
| abuse.ch Feodo Tracker, SSLBL | IP C2 ботнетов; SSL-отпечатки и IP | **Можно**: CC0, коммерческое и некоммерческое использование «без ограничений» (ToU §3.4) |
| abuse.ch URLhaus | URL вредоносов: CSV/JSON, RPZ, hostfile, раз в 5 минут; нужен Auth-Key | **Платно при коммерции**: ToU §4.1 — «may require a paid subscription» через Spamhaus; §3.1 — доступ для «not-for-profit purposes» |
| abuse.ch MalwareBazaar | хэши и образцы, get_info, пакеты раз в час и раз в сутки | то же, что URLhaus |
| abuse.ch ThreatFox | IoC (домены, IP, URL, хэши) | то же. Старый FAQ ThreatFox обещал CC0, но действующие ToU от 2025–2026 его перекрывают |
| Spamhaus ZEN, DBL, ZRD (публичные зеркала и бесплатный DQS) | IP и домены по DNS; ZRD входит в бесплатный DQS | **Нельзя без оплаты**: только «non-commercial»; использование в коммерческом устройстве требует платной подписки DQS. Пороги 100 тыс. соединений и 300 тыс. запросов в сутки есть только в старых текстах, у Spamhaus сейчас не указаны |
| Spamhaus DROP/EDROP | вредоносные сети, раз в сутки | **Можно**: «no cost, regardless of size or business type»; при использовании в продукте нужна ссылка на Spamhaus с датой и © |
| SURBL | домены из ссылок | бесплатно при <1000 пользователей или <250 тыс. писем в сутки, но «does not include embedding… in products… for which a fee is charged»; для любого продукта нужен договор SDS (платно) |
| URIBL | домены из ссылок | только «low volume», числовых порогов нет; коммерция — платный Data Feed |
| OpenPhish Community | URL, текст, раз в 12 ч | **Запрещено**: «Non-commercial use only»; ToU прямо запрещают «threat intelligence, detection… product development» и передачу третьим лицам |
| PhishTank (Cisco Talos) | URL: XML/CSV/JSON, раз в час | FAQ: коммерческое использование «OK». Но регистрация новых пользователей, по вторичным данным, закрыта с 2020 года, а без ключа — «a few downloads per day»; действует EULA Cisco |
| AlienVault OTX (LevelBlue) | пульсы IoC | **Запрещено**: «free… for non-commercial use»; нельзя «commercially exploit» и предоставлять третьим лицам «as a service bureau», то есть MSSP исключён |
| CIRCL OSINT и фиды по умолчанию в MISP | события MISP JSON | маркировка TLP:CLEAR, но лицензии на данные нет — **запросить письменно**. Сам MISP под AGPL-3.0: встраивать в код нельзя, отдельным процессом можно |
| Google Safe Browsing | URL, хэш-префиксы | **Запрещено**: «non-commercial use only» |
| Google Web Risk | то же | платный, цены н/п; локальная база хэшей, но совпадение подтверждается онлайн; ответы «must not be redistributed». Для закрытого контура не годится |
| ClamAV официальные базы | сигнатуры CVD | лицензию на базы не нашёл (н/п); движок GPLv2, поэтому libclamav не линковать, работать с clamd через сокет. Частное зеркало через cvdupdate документировано; загрузки ограничиваются по IP |
| SaneSecurity | доп. сигнатуры ClamAV (фишинг, спам) | «free of charge in commercial products», но копировать и распространять без разрешения нельзя. Заказчик тянет сам, мы не раздаём |
| Phishing.Database | домены и URL фишинга | MIT на репозиторий; лицензия данных и их происхождение не указаны — риск |
| НКЦКИ (safe-surf.ru) | бюллетени PDF и JSON по уязвимостям, IoC внутри текста | машиночитаемого фида IoC нет; «Все права защищены»; ГосСОПКА — для субъектов |
| ФинЦЕРТ (АСОИ) | обмен инцидентами и IoC | закрытый: участники финрынка и компании по ИБ финсферы; вендору можно попробовать вступить |

**Прямо запрещают коммерческое использование:** OpenPhish Community, Google Safe Browsing, AlienVault OTX, бесплатные Spamhaus ZEN/DBL/ZRD. **Требуют платной подписки при коммерции или объёме:** abuse.ch URLhaus, MalwareBazaar, ThreatFox; SURBL; URIBL.

## (г) Стартовый набор TI в три уровня (выбран владельцем)

- **Уровень 1 — сами раздаём подписанным пакетом:**
  - Feodo Tracker и SSLBL (CC0);
  - Spamhaus DROP/EDROP (с атрибуцией);
  - наши собственные признаки;
  - Public Suffix List — для «нового домена». Её лицензию в этом исследовании не проверял.
- **Уровень 2 — коннектор «ключ заказчика»:** фид включается по ключу или подписке самого заказчика, мы его не раздаём.
  - Spamhaus DQS, SURBL, URIBL, abuse.ch (URLhaus, MalwareBazaar, ThreatFox), SaneSecurity, ClamAV через частное зеркало заказчика, коммерческий OpenPhish, Web Risk.
  - Для MSSP: договор у MSSP и учёт на каждого клиента.
- **Уровень 3 — импорт по стандартам (своё TI или ГосСОПКА):** простые списки (txt, CSV, RPZ), фиды MISP (manifest.json + CSV) и клиент TAXII 2.1 / STIX 2.1 — для заказчиков с собственной TI-платформой. STIX/TAXII сам по себе подписи не даёт. В MISP есть «protected events» с PGP-подписью на каждом шаге синхронизации (с 2.4.156).
- **Подпись и офлайн:**
  - пакет — это манифест с версией и хэшами, подписанный Ed25519, как карты Rspamd;
  - перед установкой — lint и пробный прогон;
  - номер версии только растёт, предыдущие N пакетов хранятся для отката;
  - импорт из файла для закрытого контура, как `sa-update --install` или зеркало ClamAV;
  - для каждого источника — TTL и действие при устаревании: снизить вес, но не отключать молча.

**CIRCL и PhishTank.** Юридически они не прозрачны, поэтому пока не включаем их. Письменные запросы разрешения подготовлены в ti-permission-requests.md.

## (д) Источники

- abuse.ch: https://abuse.ch/terms-of-use/ ; https://urlhaus.abuse.ch/api/ ; https://bazaar.abuse.ch/api/ ; https://threatfox.abuse.ch/api/ (FAQ с CC0 — н/п в действующей версии)
- Spamhaus: https://www.spamhaus.org/organization/dnsblusage.html ; https://www.spamhaus.com/data-access/free-data-query-service/ ; https://www.spamhaus.com/faqs/data-query-service/ ; https://www.spamhaus.org/blocklists/do-not-route-or-peer/ ; https://www.spamhaus.com/resource-center/welcome-to-the-spamhaus-developer-license/
- SURBL: https://www.surbl.org/usage-policy ; URIBL: https://uribl.com/about.shtml
- OpenPhish: https://github.com/openphish/public_feed ; https://openphish.com/terms.html ; https://openphish.com/phishing_feeds.html
- PhishTank: https://phishtank.org/faq.php ; https://phishtank.org/developer_info.php ; про закрытие регистрации — https://en.wikipedia.org/wiki/PhishTank (вторичный)
- OTX: https://levelblue.com/terms/otx-eula
- MISP/CIRCL: https://www.misp-project.org/feeds/ ; https://www.misp-project.org/license/ ; https://helga.circl.lu/adulau/misp-circl-feed ; https://misp-project.org/2022/03/18/MISP.2.4.156.released.html
- Google: https://developers.google.com/safe-browsing/v4/usage-limits ; https://docs.cloud.google.com/web-risk/docs/overview ; цены Web Risk — н/п
- ClamAV: https://docs.clamav.net/appendix/CvdPrivateMirror.html ; https://docs.clamav.net/faq/faq-cvd.html ; лицензия баз — н/п. SaneSecurity: https://sanesecurity.com/legal/ ; https://sanesecurity.com/usage/
- Phishing.Database: https://github.com/Phishing-Database/Phishing.Database
- НКЦКИ: https://safe-surf.ru/specialists/bulletins-nkcki/ ; ФинЦЕРТ: https://cbr.ru/information_security/fincert/
- Rspamd: https://docs.rspamd.com/modules/multimap ; https://docs.rspamd.com/modules/phishing ; https://docs.rspamd.com/configuration/maps ; лицензия Apache 2.0 — по репозиторию github.com/rspamd/rspamd, страница лицензии в документации — н/п
- SpamAssassin: https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html
- Stalwart: https://stalw.art/docs/spamfilter/rules/ ; https://stalw.art/docs/sieve/
- Sieve: RFC 5228, 5235, 6134, 5703, 5173, 5183, 5229, 5231 (https://www.rfc-editor.org/rfc/rfcNNNN). Тексты RFC в этой сессии не перечитывал, описываю по памяти.
- Sigma: https://github.com/SigmaHQ/sigma ; https://github.com/SigmaHQ/Detection-Rule-License ; YARA-X: https://github.com/VirusTotal/yara-x
- Microsoft: https://learn.microsoft.com/en-us/exchange/security-and-compliance/mail-flow-rules/conditions-and-exceptions ; https://learn.microsoft.com/en-us/defender-xdr/custom-detection-rules
- Rust regex: https://docs.rs/regex/latest/regex/
- STIX 2.1 / TAXII 2.1: https://docs.oasis-open.org/cti/stix/v2.1/ ; https://docs.oasis-open.org/cti/taxii/v2.1/ (в этой сессии не открывал)

## (е) Дополнение при планировании 14.11

- **Public Suffix List** — проверено: заголовок `public_suffix_list.dat` — Mozilla Public License 2.0
  (https://publicsuffix.org/list/public_suffix_list.dat). Файловый копилефт: файл можно раздавать в коммерческом
  продукте без изменений с уведомлением о лицензии и ссылкой на источник; правки самого файла пришлось бы раскрыть.
  Просьба сайта — скачивать не чаще раза в сутки. Разбор — крейт `publicsuffix` 2.3 (MIT/Apache-2.0, список читается
  во время работы, поэтому обновляется пакетом, а не выпуском).
- **Что уже есть в `Cargo.lock`:** `ring` 0.17 (через `rustls`, `rustls-webpki`, `hickory-proto`) — Ed25519 подпись и
  проверка без нового пакета; `regex` 1 (транзитивно); `zip` 9, `serde_json`, `reqwest`, `winnow` 1, `ipnet`. Крейтов
  STIX нет, а найденные на crates.io (`stix` 0.3 — 2021 год, `rstix`, `stix-rs`) незрелые: STIX 2.1 — это JSON
  (`serde_json`), из языка шаблонов нужно только равенство для `ipv4-addr`, `ipv6-addr`, `domain-name`, `url`,
  `file:hashes` — разбор на `winnow`.
- **DNSBL с ключом заказчика** (Spamhaus DQS, SURBL, URIBL) у нас уже идут через Rspamd (`tp-smtp/src/screen.rs`):
  модуль `rbl` Rspamd проверяет домены ссылок (SURBL, URIBL) и IP, для DQS Spamhaus публикует готовую конфигурацию
  Rspamd с ключом заказчика (https://github.com/spamhaus/rspamd-dqs) — свой DNS-клиент для них не пишем.
- **ClamAV** у нас уже через `clamav-milter` и `clamd` по сокету (роль `clamav_milter`), libclamav не линкуется.

## (ж) Российские фиды (дополнение 14.11 по просьбе владельца)

Как искал: страницы вендоров, их условия и открытые репозитории на GitHub. Сайты bi.zone, ptsecurity.com,
rvision.ru, f6.ru, anti-malware.ru из нашей сети открываются через раз, поэтому часть сведений взята из поисковой
выдачи по страницам вендора. Такие места помечены «по выдаче». **Вывод одной строкой:** ни один российский
коммерческий фид по публичным условиям не разрешает раздавать индикаторы третьим лицам. Все они — уровень «ключ
заказчика», а раздача возможна только по отдельному OEM-договору. Государственные НКЦКИ и ФинЦЕРТ машинного фида
для вендоров не дают: их данные приходят к нам только импортом из TIP или из файла заказчика.

| Фид | Что даёт почте | Формат и доставка | Частота | Бесплатно | Коммерция, MSSP, раздача, кэш | Уровень у нас |
|---|---|---|---|---|---|---|
| RST Cloud Threat Feed | IP, домены, URL, хэши MD5/SHA-1/SHA-256 с баллом и связями (ВПО, группы, CVE); 260+ источников, ~250–300 тыс. новых в сутки | CSV, JSON (подробнее) и STIX 2.1. REST: `GET https://api.rstcloud.net/v1/{тип}?type=csv\|json\|stix&date=latest`, заголовок `X-Api-Key`, ответ gzip (по клиенту `rstapi-python`, MIT; тип `url` в примере, `ip`, `domain`, `hash` — по описанию продукта). Ещё есть STIX/TAXII (интеграция с Sentinel), Lookup API, агент выгрузки | полная выгрузка раз в 24 ч | пробный период на месяц; на GitHub `rstcloud/rstthreats` (MIT) лежат случайные 1000 бесплатных и 100 платных индикаторов — это образец, обновлялся до 19.08.2024 | Публичные ToS: §1 — только для своей деятельности, «shall not authorize any third party to access or use»; §7 — запрет перепродажи и сублицензии; §9 — лицензия «non-sublicensable». EULA §4.6 — ключ не передавать третьим лицам. Право Нового Южного Уэльса (RST Cloud Pty Ltd). В РФ, по прессе, работает ООО «Технологии киберугроз». Прецедент OEM: фиды RST встроены в NGFW «Континент 4» («Код Безопасности», 2023), значит, отдельный договор на встраивание возможен | ключ заказчика; наш ключ — стенды и демонстрации; «в поставке» — только если договор разрешит раздачу |
| RST Noise Control | вердикт «заведомо безопасно» для IP, доменов, URL и хэшей — убирает ложные срабатывания TI (CDN, публичные DNS) | REST `GET /v1/benign/lookup?value=…`, пакетно `POST /v1/benign/batch/{тип}` и `GET /v1/benign/result/{тип}`, JSON `{value, type, benign, reason}` | по запросу | нет; AWS Marketplace — $0,002 за запрос, общая подписка с IoC Lookup и Whois API | те же ToS | позже: фильтр ложных срабатываний при загрузке списков, после договора |
| RST Report Hub | отчёты об угрозах в STIX 2.1 с извлечёнными IoC | `GET /v1/reports?date=…`, `GET /v1/reports/?id=…&format=json\|stix\|pdf` | ежедневно | нет | те же ToS | не для почтового узла: это источник для TIP заказчика, к нам приходит импортом |
| Kaspersky Threat Data Feeds | Malicious URL, Phishing URL, Botnet C&C URL, Ransomware URL, Malicious Hash, IP Reputation, APT IP/URL/Hash и другие | JSON (Enterprise), plain text (OEM); утилита переводит в STIX, OpenIOC, CSV. HTTPS с сертификатом клиента; популярные фиды — TAXII 2.1 `https://taxii.tip.kaspersky.com/v2`, Basic-авторизация `taxii` + токен (коннектор OpenCTI от KasperskyLab, Apache-2.0) | URL, хэши, IP — каждые 20 мин; C&C и APT — 60 мин; демо — 24 ч | демо-фиды (IP Reputation, Botnet C&C URL, Malicious Hash и др.) с «lower detection rates»; бесплатный портал OpenTIP с ключом API и ограничением частоты (условия использования — н/п) | T&C §2.1: «solely for Customer's internal business purposes»; §3.4: передавать третьим лицам «strictly prohibited» платно и бесплатно; §3.5: нельзя использовать фиды для создания своего продукта для обнаружения угроз. Есть отдельная программа Technology Alliances и OEM: Data Streams для вендоров, plain text | ключ заказчика (TAXII 2.1 или HTTPS); раздача — только по OEM-договору |
| PT Threat Intelligence Feeds (PT ESC), портал PT Fusion | домены, URL, IP, хэши; наборы фидов: APT, фишинг, классы ВПО, severity, белые списки, CDN | REST API сервисов PT и SDK; из PT Fusion — STIX, JSON (с контекстом, для продуктов PT), CSV (по выдаче). TAXII — н/п | н/п | пилот | условия лицензии публично — н/п | ключ заказчика (REST или файл STIX/CSV в импорт) |
| BI.ZONE Threat Intelligence | 70+ млн IoC на портале, кластеры активности | готовые модули портала, STIX/TAXII 2.1 и собственный API портала | н/п | бесплатная версия портала с базовой информацией о кластерах; бесплатный пилот | «Условия приобретения»: цена индивидуально по параметрам лицензирования, правообладатель ООО «БИЗон»; про раздачу и MSSP — н/п | ключ заказчика (TAXII 2.1) |
| F6 Threat Intelligence и TI Feeds (бывший F.A.C.C.T., ранее Group-IB TI) | IP, домены, URL, хэши; 3+ млн подтверждённых индикаторов | STIX/TAXII 2 и REST API, готовые интеграции с SIEM и SOAR (блог и брошюра F6, по выдаче); версия TAXII — н/п | н/п | н/п | н/п | ключ заказчика (TAXII или REST) |
| Solar TI Feeds (ГК «Солар»: Solar JSOC, 4RAYS, сенсоры «Ростелекома») | IoC (адреса, хэши), сигнатуры Suricata, правила Sigma, YARA, ModSec | через «TI Agent» и коннекторы к EDR, SIEM, NTA (брошюра и white paper 2026, по выдаче); открытый API и TAXII — н/п | н/п | пилот | н/п | ключ заказчика через импорт (файл или TIP) |
| R-Vision TIP, Security Vision TIP | платформы заказчика, не фиды: собирают Kaspersky, BI.ZONE, F6, RST и др. (у Security Vision это из коробки), принимают TXT и CSV (у R-Vision — конструктор CSV); у R-Vision есть сервис фида ФинЦЕРТ (3.16), Security Vision получает данные НКЦКИ, ФСТЭК и ФинЦЕРТ (12.2025) | выгрузка в СЗИ через API и правила экспорта (по выдаче); TAXII-сервер — н/п | по расписанию TIP | — | условия лицензий исходных фидов действуют и после TIP | импорт: клиент TAXII 2.1, если TIP его отдаёт, иначе файл CSV, TXT или STIX в каталог импорта |
| НКЦКИ / ГосСОПКА | бюллетени и индикаторы компрометации для субъектов КИИ | приказ ФСБ от 25.12.2025 № 548 (в силе с 30.01.2026): обмен через личный кабинет в технической инфраструктуре НКЦКИ; API с токеном организации (так его подключают KUMA и Kaspersky XDR); инциденты — JSON. Формат IoC в API публично — н/п; на safe-surf.ru — бюллетени PDF и JSON | по мере выпуска | для субъектов КИИ и центров ГосСОПКА | третьим лицам не предназначено; раздавать нельзя; для вендора доступа нет | импорт: TIP заказчика (STIX/TAXII, MISP) или файл |
| ФинЦЕРТ Банка России (АСОИ ФинЦЕРТ) | сведения об угрозах и атаках, рекомендации, база переводов без согласия клиента | личный кабинет `lk.fincert.cbr.ru`, API `api.fincert.cbr.ru`, отдельная учётная запись и TLS-сертификат сотрудника, крипто-API для электронной подписи (инструкция ЦБ по API, по выдаче). Подключение — карточка участника на info_fincert@cbr.ru и материалы `ASOI_docs.zip`. STIX — н/п | по мере выпуска | для участников финрынка и компаний ИБ финсферы; разработчикам ПО учётки для проверки интеграции — индивидуально | раздавать нельзя; правила TLP публично — н/п | импорт: TIP заказчика (R-Vision и Security Vision его принимают) или файл |
| Гарда TI Feeds, Angara (ACRC) и другие | Гарда — коммерческие фиды; Angara — подключение к ГосСОПКА как центр | н/п | н/п | н/п | н/п | при запросе заказчика — импорт |

**Что следует для архитектуры.**
- Своих протоколов у российских вендоров почти нет. Kaspersky, BI.ZONE и F6 дают TAXII 2.1 / STIX 2.1, RST отдаёт
  STIX 2.1 по REST, остальные — REST или файлы STIX и CSV. Поэтому отдельный парсер на каждого вендора не нужен:
  коннектор — это пресет к уже запланированным `Taxii` и `Http` с форматом `Stix21` или `Csv`. Различаются только
  способы авторизации: заголовок (`X-Api-Key` у RST, `Auth-Key` у abuse.ch), Basic (Kaspersky TAXII: `taxii` +
  токен), Bearer и сертификат клиента (Kaspersky HTTPS).
- Сама TI-платформа заказчика (R-Vision, Security Vision, Kaspersky CyberTrace) — главный путь для ГосСОПКА
  и ФинЦЕРТ. Прямой клиент к НКЦКИ или АСОИ не пишем: доступ есть только у субъекта, формат IoC закрыт, нужна
  отечественная криптография, а данные нельзя выносить из организации.
- **RST Cloud с нашим ключом.** По публичным условиям раздавать индикаторы нельзя, ключ третьим лицам не передаётся,
  а автоматическое «periodic caching» запрещено (§7). Значит, до отдельного договора наш ключ годится только для
  наших стендов и демонстраций. Для уровня «в поставке» в договоре должно быть прямо написано:
  (1) разрешена раздача индикаторов заказчикам в составе нашего продукта, в том числе в закрытый контур;
  (2) разрешено хранить и раздавать снимок списков;
  (3) атрибуция;
  (4) поведение после окончания договора: §9 ToS требует удалить ПО;
  (5) MSSP;
  (6) применимое право — российское юрлицо или австралийское.
  Прецедент «Континента 4» показывает, что такой договор возможен.

**Источники (ж).**
- RST Cloud: https://www.rstcloud.com/terms ; https://www.rstcloud.com/eula ; https://www.rstcloud.com/rst-threat-feed/ ;
  https://www.rstcloud.com/rst-noise-control/ ; https://www.rstcloud.com/ ; https://github.com/rstcloud/rstapi-python
  (`rstapi/threatfeed.py`, `noisecontrol.py`, `reporthub.py`) ; https://github.com/rstcloud/rstthreats (`feeds/README.md`) ;
  «Континент 4» и RST — https://www.anti-malware.ru/news/2023-07-18-111332/41582 (вторичный)
- Kaspersky: https://media.kaspersky.com/en/Terms_and_Conditions_Threat_Data_Feeds.pdf ; https://support.kaspersky.com/13849 ;
  https://github.com/KasperskyLab/threat-intelligence (`kaspersky-feeds-for-opencti`) ; https://www.kaspersky.com/partners/technology/feeds ;
  https://opentip.kaspersky.com/Help/Doc_data/AboutTermsOfUse.htm (текст условий OpenTIP не открыл — н/п)
- Positive Technologies: https://global.ptsecurity.com/products/threat-intelligence-feeds ;
  https://help.ptsecurity.com/en-US/projects/feeds/2.1.0/help (не открылась) ; презентация вебинара PT Feeds, 2023
  (https://www.ptsecurity.com/upload/corporate/ru-ru/webinars/ics/Prezentaciya-pt-feeds-kak-povysit-zashchishchennost-pri-pomoshchi-ti.pdf) ;
  PT Fusion 1.5 — https://www.securitylab.ru/news/570503.php (вторичный)
- BI.ZONE: https://bi.zone/catalog/products/threat-intelligence/ ; «Условия приобретения» (PDF на той же странице)
- F6: https://www.f6.ru/products/threat-intelligence/ ; https://www.f6.ru/products/threat-intelligence-feeds/ ; https://www.f6.ru/blog/ti-siem/ (по выдаче)
- Солар: https://rt-solar.ru/upload/iblock/aa8/nnydzvxmeff7gfxhx2k286v8xfjcdbm0/Brochure-Solar-Threat-Feeds.pdf (по выдаче)
- R-Vision TIP 3.16 и фид ФинЦЕРТ — https://www.anti-malware.ru/news/2023-12-06-111332/42425 ; Security Vision и НКЦКИ, ФСТЭК,
  ФинЦЕРТ — https://www.anti-malware.ru/news/2025-12-17-111332/48458 (вторичные)
- НКЦКИ: приказ ФСБ России от 25.12.2025 № 548 (обзор — https://ppt.ru/obzory/vstupaet-v-silu/prikaz-fsb-25-12-2025-548-o-poryadke-vzaimodeystviya-subektov-kii-s-gossopka) ;
  интеграция KUMA — https://support.kaspersky.com/help/KUMA/3.0.1/ru-RU/221777.htm ; Kaspersky XDR — https://support.kaspersky.com/help/XDR/1.4/ru-RU/252732.htm
- ФинЦЕРТ: https://cbr.ru/information_security/fincert/ ; инструкция по API АСОИ ФинЦЕРТ (копия у стороннего разработчика —
  docs-beta.brainysoft.ru, не открылась; по выдаче)
- Не найдено (н/п): цены RST и всех российских вендоров; условия MSSP и раздачи у PT, BI.ZONE, F6, Солара; TAXII у PT
  и Солара; TAXII-сервер у R-Vision и Security Vision; формат IoC в API НКЦКИ; STIX и TLP в АСОИ ФинЦЕРТ; условия OpenTIP.
