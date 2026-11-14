# SMTP-51: ответ на наше письмо — метка в Message-ID, BATV, признак ответа, аномалии беседы

lane: smtp
deps: [CTR-94, DEPS-18, SMTP-50]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/research/2026-11-14-replies-threads.md, docs/tasks/CTR-94.md, docs/tasks/SMTP-50.md]
checks: [nextest]
manual: no

## Сделать
- Метка Message-ID: левая часть — XChaCha20-Poly1305 (DEPS-18) от случайного значения и времени, с id ключа
  открытым префиксом (`tp1.<id>.<base32>@<домен>`); шифруется, чтобы не раскрывать время и ящик. Ставится, где
  Message-ID задаёт сервер: `Delivery::submit` и письмо submission без Message-ID, автоответы, DSN, iMIP узла.
  Message-ID клиента не заменяется (решение 125 в). Запись об отправленном (`SentLog::record`) — при постановке в
  очередь письма с внешними или партнёрскими адресатами.
- BATV (draft-levine-smtp-batv-01): `MAIL FROM` исходящих наружу — `prvs=KDDDSSSSSS=local@domain` (K — номер ключа, DDD
  — день истечения, SSSSSS — первые 3 байта HMAC-SHA1); считается один раз при постановке и хранится в записи очереди —
  повторы и greylisting видят ту же метку; срок 7 дней (`smtp.batv.valid_for`); без метки — SRS-пересылки, DSN `<>`,
  адреса и домены `smtp.batv.exempt` (рассылки, ezmlm). Приём: письмо с пустым `MAIL FROM` на локальный адрес без
  валидной метки (нет, просрочена, чужой ключ) — `550 5.7.1` на `RCPT`; с валидной — метка снимается, доставка; режимы
  `smtp.batv.mode: log|enforce` для перехода.
- Признак ответа на smtp-in: `In-Reply-To`, затем `References` — метка расшифрована (ключ известен, время не в будущем)
  или хэш Message-ID найден; затем запись: отправитель `From` — среди адресатов, DMARC pass с выравниванием
  (RFC 9989), возраст ≤ `smtp.reply.max_age` (30 дней). Все три — `PeerContext.reply = true` (CTR-94) для матрицы
  доверия и правил; иначе `false`. Заголовок `X-ThePost-Reply` письма извне удаляется до проверок; при признаке узел
  ставит свой — до Milter, Rspamd снижает балл (DEP-56).
- Антивирус, URL и DMARC — как для всех: признак ответа ничего не пропускает; `quarantine` и отказ фильтра действуют.
- Аномалии в доверенной беседе (память беседы, CTR-94): новый для беседы тип вложения; зашифрованный архив (ZIP с
  флагом шифрования, 7z и RAR с зашифрованным заголовком); ISO, IMG, VHD, LNK по типу и расширению; ссылка на новый для
  беседы домен; `Reply-To` не совпадает с `From` и прежним `Reply-To` беседы — признак снимается, заголовок
  `X-ThePost-Reply-Anomaly: <причины>` (Rspamd повышает балл), событие журнала доставки.
- Конфигурация — секции `smtp.reply` и `smtp.batv` с `Default` (SRS 5.5); метрики признака, отказов BATV и аномалий.

## Как делают другие
- Rspamd `replies` (−4 за ответ; вариант без БД — `cookie` в Message-ID, срок 2 недели), `known_senders`
  (https://docs.rspamd.com/modules/replies, https://docs.rspamd.com/modules/known_senders).
- BATV и SRS (https://datatracker.ietf.org/doc/html/draft-levine-smtp-batv-01,
  https://en.wikipedia.org/wiki/Bounce_Address_Tag_Validation); SpamAssassin VBounce
  (https://metacpan.org/pod/Mail::SpamAssassin::Plugin::VBounce).
- Allow-list не отключает антивирус и high-confidence phishing: Microsoft
  (https://learn.microsoft.com/en-us/defender-office-365/create-safe-sender-lists-in-office-365), Mimecast Auto Allow с
  2023 — снова спам-проверка; first contact и аномалии — Defender
  (https://learn.microsoft.com/en-us/defender-office-365/anti-phishing-policies-about).
- Атаки на доверие к ответу: Emotet (https://unit42.paloaltonetworks.com/emotet-thread-hijacking/), Squirrelwaffle и
  Qakbot (https://www.trendmicro.com/en_us/research/21/k/Squirrelwaffle-Exploits-ProxyShell-and-ProxyLogon-to-Hijack-Email-Chains.html),
  IcedID (https://intezer.com/blog/conversation-hijacking-campaign-delivering-icedid), TA577
  (https://www.proofpoint.com/us/blog/threat-insight/ta577s-unusual-attack-chain-leads-ntlm-data-theft), MITRE T1534
  (https://attack.mitre.org/techniques/T1534/).
- Отличие: признак требует трёх условий сразу и снимается аномалией; вечного auto-allow нет.

## Приёмка
- Тесты: ответ внешнего адресата на наше письмо с DMARC pass — признак есть; от не-адресата, с DMARC fail, старше 30
  дней, с подделанной меткой или чужим ключом — нет; ответ на письмо клиента с его Message-ID — по хэшу; входящий
  `X-ThePost-Reply` удалён.
- BATV: метка одинакова в повторах одного письма; bounce на адрес без метки — `550 5.7.1` в `enforce`, только журнал в
  `log`; на валидную — доставлен без метки в адресе; адрес из `exempt` — без метки.
- Аномалии: ответ с ISO, зашифрованным ZIP, новым доменом ссылки, сменой `Reply-To` — без признака и с заголовком
  аномалии; ответ с вирусом — карантин при любом признаке.
