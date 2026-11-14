# Ответ на наше письмо как признак доверия, беседа как единица
Исследование 14.11.2026 для решения 125. Владелец принял пакет из раздела (б).

**Коротко.** Идея владельца в индустрии уже есть, причём в двух видах:
- **Со состоянием на сервере.** Это модуль `replies` в Rspamd: сервер запоминает хэши Message-ID исходящих писем, и входящий ответ с совпавшим In-Reply-To получает −4 к спам-баллу.
- **Без состояния.** Это зашифрованная метка-«cookie» в Message-ID у того же Rspamd и BATV для MAIL FROM. По сути это аналог SYN-cookies, а не stateful-таблицы.

Списки разрешённых по исходящей почте тоже распространены: Mimecast Auto Allow, Outlook «добавлять адресатов в Safe Senders», mailbox intelligence в Defender, known senders в Google Calendar. Почти у всех вендоров ответ всё равно проходит проверку на вирусы.

Атаки Emotet, Qakbot, IcedID, Squirrelwaffle и TA577 построены ровно на «доверии к ответу». Поэтому ответ — повод снизить спам-балл, но не отключать проверки. Транзакционной семантики для беседы нет ни у кого.

## 1. Что уже есть в протоколах и практике

**BATV и SRS.**
- BATV — истёкший черновик IETF draft-levine-smtp-batv-01 (2008), не RFC. Формат: `prvs=KDDDSSSSSS=user@domain`, где K — номер ключа, DDD — день истечения (рекомендуемый срок 7 дней), SSSSSS — первые 3 байта HMAC-SHA1.
- Bounce (письмо с пустым MAIL FROM) на адрес без метки или с неверной меткой отклоняется кодом 550 прямо в SMTP-сессии.
- Ограничения из самого черновика: слабая защита от повтора (replay), метка не привязана к содержимому письма. Wikipedia добавляет: метка должна быть стабильной при повторных попытках доставки (greylisting), а некоторые менеджеры рассылок (ezmlm) перестают узнавать изменённый адрес.
- SRS (Shevek, Mail::SRS) — тот же приём для пересылки: `SRS0=HHH=TT=domain=user@forwarder` с усечённым HMAC-SHA1 и меткой времени. Отдельного RFC нет.
- SpamAssassin VBounce отличает настоящие bounce на нашу почту по своим релеям в Received.

**Списки разрешённых из исходящей почты.**
- **Mimecast Auto Allow.** Включён по умолчанию, записи создаются, когда внутренний пользователь пишет внешнему адресату. Такой отправитель пропускает проверку IP-репутации, спам-проверки и greylisting. Антивирус обойти нельзя. С 2023 года есть «Auto Allow Spam Scanning» против BEC. Страницы Mimecast отдают 403, поэтому это сведения по выдаче поиска.
- **Microsoft Outlook.** Флажок «Automatically add people I e-mail to the Safe Senders List». В EOP запись из Safe Senders даёт `SFV:SFE`: пропускаются спам-, spoof- и phish-фильтры, но не high-confidence phishing. Вредоносные письма и high-confidence phishing уходят в карантин всегда, при любом allow-list. Microsoft ставит этот метод на третье место из пяти и прямо пишет: «the sender can still be compromised». Обход фильтров также мешает ZAP.
- **Microsoft Defender.** Mailbox intelligence строит граф контактов. Если отправитель и получатель уже переписывались, защита от выдачи себя за пользователя или домен (impersonation) и защита mailbox intelligence не срабатывают. First contact safety tip, наоборот, предупреждает о первом или редком контакте.
- **Google Calendar.** Режим «Only if the sender is known» (2022). Известными считаются контакты, свой домен и те, с кем пользователь уже взаимодействовал. Приглашение от неизвестного приходит письмом, но не попадает в календарь.
- **Proofpoint, Barracuda, Cisco.** Автоматический список разрешённых по исходящей почте в документации не найден (н/п). У Barracuda исключения настраивает администратор: обходят спам-скоринг, но не антивирус. Proofpoint Essentials Safe Sender даёт пометку «Filtered: Allow».

**Привязка ответа к нашему письму (RFC 5322 §3.6.4).**
- Message-ID «MUST be globally unique», In-Reply-To и References строит клиент отправителя. Целостность этих заголовков ничем не защищена.
- Rspamd `replies`:
  - хранит хэши Message-ID писем от авторизованных и локальных отправителей в Redis, срок хранения 1 день;
  - на входящее письмо с совпавшим In-Reply-To ставит символ `REPLY` (−4);
  - заносит адресатов в списки «verified senders»; `known_senders` даёт `INC_MAIL_KNOWN_*` (−1);
  - опционально задаёт action через pre-result, по умолчанию выключено.
- Вариант без БД: опции `cookie`/`cookie_key`/`cookie_valid_time` (по умолчанию 2w). Это «DKIM-like reply cookie in message-id»: зашифрованная метка с временем, проверяется расшифровкой левой части In-Reply-To, будущие и просроченные отбрасываются. Это и есть «Message-ID с HMAC» в реальном продукте.
- Ещё примеры: GitLab держит ключ ответа в адресе (`incoming+%{key}@`), References использует только для привязки к обсуждению. Helpthread — HMAC-SHA256 в Message-ID. Стандарта нет.

**Беседы.**
- **RFC 5256 THREAD.** REFERENCES — дерево по References с запасным вариантом через In-Reply-To, затем склейка по base subject. ORDEREDSUBJECT — только по теме. В разделе безопасности сказано, что поддельный References может приклеить тред к чужому.
- **RFC 8621 (JMAP Thread).** Алгоритм не обязателен. Рекомендация: общий id в Message-Id, In-Reply-To или References плюс совпадение темы без префиксов. threadId неизменяем, при слиянии письма удаляются и вставляются заново с новым id.
- **Exchange.** Thread-Index = base64 от PidTagConversationIndex: 22-байтный заголовок с FILETIME и GUID, каждый ответ дописывает 5 байт. ConversationId — это GUID из индекса (MS-OXOMSG 2.2.1.2–2.2.1.4). Значение задаёт клиент, его можно скопировать.
- **Gmail.** Тред складывается из References/In-Reply-To и темы. С 2019 года References обязан ссылаться на реальные предыдущие письма.
- **Надёжность.** Это эвристика для удобства. Ветвления и смена темы её ломают. Purview eDiscovery поэтому строит свои «inclusive» цепочки, а не верит группировке Outlook.

## 2. Атаки на «доверие к ответу»

- **Emotet.**
  - Talos: Emotet крадёт переписку и отвечает в непрочитанные треды, цитируя настоящие тела писем. В апреле 2019 так было оформлено 8,5% попыток заражения, позже — около четверти. Talos пишет, что из-за этого у антиспама «no simple pattern».
  - Unit 42: от заражения до прихода ответа с макросным документом в украденном треде прошло 1 ч 51 мин.
- **Qakbot, Squirrelwaffle, IcedID.**
  - Trend Micro (2021): после взлома Exchange через ProxyLogon/ProxyShell ответы рассылались изнутри с настоящих ящиков. Цитата: «mail gateways will not be able to filter or quarantine any of these internal emails».
  - Intezer (2022, IcedID): ответы шли с реального украденного ящика, внутри — ZIP с паролем, в нём ISO.
- **TA577 (Proofpoint, 2024).** Десятки тысяч писем в виде ответов в треды, с уникальными для каждого получателя ZIP/HTML для кражи NTLM. TA577 связана с последующим заражением вымогателем Black Basta.
- **Поддельный In-Reply-To без взлома.** Ironscales: бесплатный ящик, «RE:» и выдуманные Message-ID в References. Подделку выдало то, что такого письма в ящике получателя не было.
- **Компрометация ящика собеседника.** MITRE T1534: письмо с настоящего ящика проходит SPF, DKIM и DMARC и попадает в беседу. Именно так обходится «не проверяем ответ». Привязка к Message-ID это не ловит: у взломщика есть вся переписка, а Message-ID видят все участники, пересылки и CC.
- **Как вендоры снижают риск.** Ответ остаётся проверенным:
  - Rspamd даёт только минус в баллах;
  - Microsoft всегда фильтрует вредоносные письма и high-confidence phishing;
  - Mimecast всегда проверяет антивирусом и добавил спам-проверку для Auto Allow;
  - Barracuda проверяет вирусы и у исключённых адресатов.

  Доверие ограничено по времени: Rspamd 1 день или 2 недели, BATV 7 дней. Оно привязано к адресу (адресаты исходного письма) и к подписи (cookie или HMAC). Поверх доверия работают признаки «first contact» и аномалий: смена Reply-To, новые реквизиты, архив с паролем.

## 3. Беседа как единица («транзакция»)

**Где беседа уже объект политик:**
- **Google Vault.** Если хотя бы одно письмо треда удерживается правилом или hold, удерживается весь тред.
- **Exchange EWS ApplyConversationAction.** Действия над всеми письмами беседы, включая будущие: AlwaysMove, AlwaysDelete, категории.
- **Purview eDiscovery и Proofpoint Supervision/Discover.** Треды служат для просмотра и проверки: отметить или пропустить «одним шагом».

Журналирование, DLP и модерация в Exchange работают по отдельным письмам, retention — по элементам и папкам. Источник по Exchange — общие знания, отдельного первоисточника не искал.

**Транзакционной семантики для беседы нет ни у кого.** Ближайшие аналоги:
- JMAP /set: атомарна каждая запись, весь вызов — нет («permissible ... commit changes to some objects but not others»), есть `ifInState`;
- IMAP MULTIAPPEND (RFC 3502): атомарная загрузка пачки писем;
- календарь: состояние задаёт iTIP, но RFC 5546 §6.2.1 требует аутентифицировать отправителя и сопоставлять его с ATTENDEE.

**Полезно серверу:**
- беседа как объект JMAP Thread;
- hold и retention по треду в стиле Vault;
- выгрузка для eDiscovery и действия «игнорировать / всегда перемещать» по беседе;
- внутри — одна транзакция БД «письмо + привязка к треду + применённые политики»;
- атомарный импорт.

---

## Итог

**(а) Что из идеи уже стандартно и как называется**
- «Ответ на наше письмо — легитимен»: Rspamd `replies` (символ REPLY, stateful-таблица Message-ID) и `known_senders`; auto-allow / «известные отправители» у Mimecast, Outlook и Google Calendar; mailbox intelligence / contact graph в Defender.
- Вариант без состояния на сервере: reply cookie в Message-ID (Rspamd), BATV (`prvs=`) для bounce, SRS для пересылки. Аналогия точнее с SYN-cookies, чем со stateful-инспекцией.
- В протоколах: календарь — iTIP (RFC 5546, RFC 6638). Почта — только In-Reply-To и References (RFC 5322) без целостности. Беседа — RFC 5256 THREAD, RFC 8621 Thread, Exchange ConversationIndex. Всё это эвристика, не транзакция.

**(б) Что брать в The Post**
1. **Ответ на наше письмо — признак доверия, не исключение из проверок.** Это отрицательный вклад в спам-балл и условие для правил матрицы (например, не применять greylisting/RBL, ослабить «first contact»). Срабатывает, только если выполнено всё сразу:
   - DMARC pass с выравниванием;
   - отправитель — один из адресатов исходного письма (сверка с записью об отправленном);
   - возраст исходного письма не больше N дней, предлагаю 14–30.
2. **Проверяемая привязка через Message-ID.** Левая часть — зашифрованная или подписанная метка (как Rspamd cookie): случайное значение, время, id ключа, усечённый HMAC или AEAD. Плюсы: проверка без БД, ротация ключей, срок жизни. Шифровать, а не просто подписывать, чтобы не раскрывать время и id ящика. Метка доказывает только «это наше письмо и оно свежее», но не «отвечает тот самый человек». Поэтому проверка адресата по записи в БД обязательна.
3. **BATV (`prvs=`) в MAIL FROM исходящих** и отклонение в SMTP-сессии bounce на адреса без валидной метки. Метка стабильна при повторных попытках доставки, срок 7 дней, список исключений для рассылок.
4. **Признаки аномалий в «доверенной» беседе** повышают риск, даже если ответ привязан:
   - новый тип вложения, архив с паролем, ISO/LNK;
   - ссылка на новый домен;
   - смена Reply-To;
   - смена реквизитов;
   - отправка изнутри с необычного клиента.
5. **Беседа — объект JMAP Thread** с неизменяемым threadId и единица политик: hold и retention на весь тред, выгрузка, действия по беседе, атомарная запись «письмо + тред + политики» в одной транзакции PostgreSQL. Журналирование и DLP — по отдельным письмам.
6. **Календарь** — как решено (UID, ORGANIZER, ATTENDEE), плюс аутентификация отправителя (DMARC) по RFC 5546 §6.2.1.

**(в) Что брать нельзя и почему**
- **Не пропускать антивирус, песочницу и проверку URL для ответов.** Emotet, Qakbot, IcedID, Squirrelwaffle и TA577 доставляли нагрузку именно ответами в настоящие треды с настоящих ящиков (Talos, Unit 42, Trend Micro, Intezer, Proofpoint). Microsoft и Mimecast антивирус для allow-list не отключают.
- **Не пропускать DMARC и SPF по признаку «это ответ».** In-Reply-To и References подделываются (RFC 5256, раздел безопасности; случай Ironscales). Message-ID утекает через CC, пересылки и цитаты.
- **Не делать вечный auto-allow по факту переписки.** Microsoft прямо предупреждает, что доверенный отправитель может быть скомпрометирован. Mimecast в 2023 году вернул спам-проверку в Auto Allow из-за BEC. Отключение impersonation-защиты в Defender при прошлой переписке — ровно та дыра, которой пользуется компрометация ящика собеседника (MITRE T1534).
- **Не делать «беседу-транзакцию» основой для решений безопасности.** Принадлежность к беседе вычисляется эвристически по полям, которые задаёт клиент: Thread-Index и References копируются, беседы ветвятся. Атомарности по беседе нет ни в JMAP, ни в EWS. «Одна сессия» в почте не существует: ответы приходят через дни и по разным SMTP-соединениям.
- **Не считать доверие внутри беседы разрешением для исходящего DLP.** Ответ во внешний тред не значит, что данные можно отправлять.

**(г) Источники**
- BATV: https://datatracker.ietf.org/doc/html/draft-levine-smtp-batv-01 ; https://en.wikipedia.org/wiki/Bounce_Address_Tag_Validation
- SRS: https://metacpan.org/release/SHEVEK/Mail-SRS-0.25/view/lib/Mail/SRS.pm ; https://en.wikipedia.org/wiki/Sender_Rewriting_Scheme (RFC — н/п)
- SpamAssassin VBounce: https://metacpan.org/pod/Mail::SpamAssassin::Plugin::VBounce
- Rspamd replies: https://docs.rspamd.com/modules/replies ; known_senders: https://docs.rspamd.com/modules/known_senders ; код cookie: https://github.com/rspamd/rspamd/blob/master/src/plugins/lua/replies.lua
- Mimecast Auto Allow: https://mimecastsupport.zendesk.com/hc/en-us/articles/34000409185043 ; https://mimecastsupport.zendesk.com/hc/en-us/articles/34000704345363 ; https://mimecastsupport.zendesk.com/hc/en-us/articles/34000630679059 ; https://mimecastsupport.zendesk.com/hc/en-us/articles/34000377381907 (403 при чтении, по выдаче поиска)
- Outlook auto-add: https://support.microsoft.com/en-us/office/add-recipients-to-the-safe-senders-list-in-outlook-be1baea0-beab-4a30-b968-9004332336ce
- Microsoft allowlists: https://learn.microsoft.com/en-us/defender-office-365/create-safe-sender-lists-in-office-365 ; anti-phishing и mailbox intelligence, first contact: https://learn.microsoft.com/en-us/defender-office-365/anti-phishing-policies-about
- Google Calendar known senders: https://workspaceupdates.googleblog.com/2022/07/invitations-from-known-senders-only-google-calendar.html
- Proofpoint auto-allow по исходящей — н/п; Barracuda — н/п (исключения: https://campus.barracuda.com/doc/96023020); Cisco — н/п
- RFC 5322: https://www.rfc-editor.org/rfc/rfc5322.html ; RFC 5256: https://www.rfc-editor.org/rfc/rfc5256.html ; RFC 8621: https://www.rfc-editor.org/rfc/rfc8621.html ; RFC 8620: https://www.rfc-editor.org/rfc/rfc8620.html ; RFC 3502: https://www.rfc-editor.org/rfc/rfc3502.html ; RFC 5546: https://www.rfc-editor.org/rfc/rfc5546.html
- Exchange ConversationIndex и ConversationId: https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxomsg/9e994fbb-b839-495f-84e3-2c8c02c7dd9b ; https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxomsg/f8e4a5fa-cb73-4eca-b174-72e1fda821a6 ; https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxcmail/1b52b014-af7d-41f8-9dc5-4b0b20294457
- Gmail threads: https://developers.google.com/gmail/api/guides/threads ; https://workspaceupdates.googleblog.com/2019/03/threading-changes-in-gmail-conversation-view.html
- EWS ApplyConversationAction: https://learn.microsoft.com/en-us/exchange/client-developer/web-service-reference/applyconversationaction-operation
- Purview email threading: https://learn.microsoft.com/en-us/purview/ediscovery-email-threading
- Google Vault, тред целиком: https://support.google.com/vault/answer/2535539
- Proofpoint Supervision и Discover: https://www.proofpoint.com/us/products/digital-communications-governance/supervision ; https://www.proofpoint.com/uk/products/archiving-and-compliance/discover
- GitLab incoming email: https://docs.gitlab.com/administration/incoming_email/ ; Helpthread (HMAC в Message-ID): https://github.com/Helpthread/helpthread/pull/8
- Emotet: https://blog.talosintelligence.com/stolen-emails-reflect-emotets-organic/ ; https://unit42.paloaltonetworks.com/emotet-thread-hijacking/
- Squirrelwaffle, Qakbot, ProxyShell: https://www.trendmicro.com/en_us/research/21/k/Squirrelwaffle-Exploits-ProxyShell-and-ProxyLogon-to-Hijack-Email-Chains.html ; https://fortiguard.com/threat-signal-report/4320
- IcedID: https://intezer.com/blog/conversation-hijacking-campaign-delivering-icedid
- TA577: https://www.proofpoint.com/us/blog/threat-insight/ta577s-unusual-attack-chain-leads-ntlm-data-theft
- Поддельный тред: https://ironscales.com/threat-intelligence/gmail-thread-fabrication-zero-payload-law-firm-reconnaissance (вендорский блог)
- MITRE T1534: https://attack.mitre.org/techniques/T1534/

**Чего нет в первоисточниках:**
- Retention в Exchange по элементам и папкам, журналирование и DLP по отдельным письмам — из общих знаний.
- Как Rspamd создаёт cookie: в коде модуля есть только проверка (`decrypt_cookie`), генерация происходит вне модуля.
- Поведение pre-result в Rspamd (что именно пропускается) по документации не проверено.
