# Справочник аномалий и бесплатный TI
Исследование 14.11.2026 для решения 126. Владелец выбрал: каталог признаков + Sieve vnd.thepost.risk; TI в три уровня.

**Главное.**
- Ни один готовый формат не даёт сразу три вещи: «признак + вес», безопасное расширение администратором и подписанные обновления. Ближе всего модели Rspamd (символы, баллы, карты с подписью) и Stalwart (вендорский набор правил плюс локальные правила, которые обновление не трогает).
- Стандартный Sieve не умеет считать: в нём нет арифметики и весов. Нужно своё расширение.
- По TI картина хуже, чем принято считать. Большинство «бесплатных» фидов бесплатны только для некоммерческого использования. Явно разрешены в коммерческом продукте лишь Feodo Tracker, SSLBL, Spamhaus DROP, PhishTank и SaneSecurity (у SaneSecurity с оговорками).

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
