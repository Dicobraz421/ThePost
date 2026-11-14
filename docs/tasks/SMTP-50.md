# SMTP-50: матрица доверия в SMTP — тип абонента, автоответ, MDN, DSN, группы, автопересылка

lane: smtp
deps: [CTR-93, SMTP-49]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/research/2026-11-14-trust-matrix.md, docs/tasks/CTR-93.md, docs/tasks/SMTP-49.md, docs/ADMIN-GUIDE.md#матрица-по-умолчанию]
checks: [nextest]
manual: no

## Сделать
- Тип абонента — для каждого письма, а не только для iTIP (SMTP-49): submission со входом и `Delivery::submit` — `Own`;
  smtp-in — `Partner`, если домен `From` — партнёр организации получателя (`TrustStore::partner_of`) и DMARC pass с
  выравниванием (RFC 9989), `Neighbor` — домен другой организации установки, иначе `External`. Получатель исходящего —
  по домену адреса тем же правилом. Заголовкам `Authentication-Results` не от своего шлюза и `remote-host` не доверять
  (RFC 8601 §5, RFC 5183). Действующая матрица получателя — `TrustStore::effective_for`; ошибка чтения —
  `TrustMatrix::closed()` и запись в журнал узла, письмо доставляется.
- Автоответ (RFC 3834): тип и матрица передаются правилам (`tp_contracts::sieve`, CTR-93); чужому — только внешний текст
  и только при `auto_reply = On`; внутренний текст наружу не уходит. Остаётся: конверт `<>`, `Auto-Submitted:
  auto-replied`, ответ только на `Return-Path` (§4), не на автоматические письма, списки и пустой отправитель (§2),
  период одному отправителю по умолчанию 7 дней (RFC 5230 §4.1). Новое: письмо, пришедшее через группу рассылки, —
  без автоответа членов группы.
- MDN (RFC 8098): сервер уведомлений о прочтении не создаёт. Отчёт `multipart/report;
  report-type=disposition-notification` из submission и `Delivery::submit` получателю, у типа которого `mdn = Off`, —
  не отправляется этому получателю (`250` клиенту, событие журнала доставки «MDN не отправлен: политика»).
- DSN (RFC 3464, RFC 5321): отказы по политике (закрытая группа, неизвестный адрес) — в сессии до `250` на `RCPT`, без
  уведомления потом (§6.2); принятое и недоставленное — на адрес конверта, не на `From` (RFC 3464 §2); на письма в
  «Спаме» и карантине фильтра — без DSN. Получателю DSN не `Own`: `Diagnostic-Code` — только код и общий текст, без
  имён узлов, внутренних адресов после раскрытия групп и имён фильтров; `Reporting-MTA` — внешнее имя узла.
- Группы рассылки: `group_post` по типу отправителя (уровень группы — политика самой группы, иначе организации):
  `Closed` — `550 5.7.1` на `RCPT`; `Moderated` — в очередь модерации; `Open` — как сейчас. Ограничения отправителей
  группы сужают и дальше. Отказ модератора чужому отправителю — без уведомления (только журнал доставки).
- Автопересылка: `redirect` правил и адреса пересылки модулей (`CopyTo::Address`) на внешний адрес — только при
  `auto_forward = On` для типа получателя или домене из `auto_forward_domains`; иначе не пересылается, письмо остаётся
  в ящике, событие журнала доставки. Почтовые контакты в составе группы — не автопересылка.
- iTIP: итог `processcalendar` правил (CTR-93) передаётся приёму iTIP (`ItipDelivery`) вместе с типом отправителя.
- Сбой скрипта организации при выполнении (SIEVE-14) — `451 4.3.0` в сессии smtp-in и `SubmitStatus::Failed` с тем же
  кодом в `Delivery::submit`; сбой скрипта пользователя — как сейчас, во «Входящие».

## Как делают другие
- Автоответ внешним — отдельный текст и аудитория: Exchange `ExternalAudience`, `ExternalMessage`
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-mailboxautoreplyconfiguration); члены
  группы не отвечают отправителю — `SendOofMessageToOriginatorEnabled`
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-distributiongroup); Zimbra — раз в 7 дней.
- Группы закрыты для внешних: Exchange `RequireSenderAuthenticationEnabled=$true`
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-distributiongroup), Google
  (https://knowledge.workspace.google.com/admin/groups/set-organization-wide-policies-for-using-groups).
- Backscatter: EOP молча отбрасывает сомнительное без NDR
  (https://learn.microsoft.com/en-us/defender-office-365/anti-spam-backscatter-about); NDR и автопересылка по
  удалённому домену — `Set-RemoteDomain` (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-remotedomain).
- Автопересылка наружу по умолчанию запрещена политикой исходящего спама
  (https://learn.microsoft.com/en-us/defender-office-365/outbound-spam-policies-external-email-forwarding).
- MDN по OU, «внутри и список внешних»: Google
  (https://knowledge.workspace.google.com/admin/gmail/let-users-request-read-receipts).
- Отличие: MDN, отклонённый политикой, не возвращается пользователю ошибкой — это машинный отчёт его программы.

## Приёмка
- Тесты: письмо smtp-in с доменом партнёра и DMARC fail — `External`; с адресом своего домена — `External`;
  `Authentication-Results` чужого сервера не меняет тип.
- Автоответ: внешнему без внешнего текста — ответа нет; с внешним текстом — внешний; своему — внутренний; письмо через
  группу — без ответа; `Auto-Submitted: auto-replied` и конверт `<>` в ответе.
- MDN из submission внешнему при `mdn = Off` — не в очереди, событие в журнале; своему — доставлен.
- Группа `Closed` — `550 5.7.1` на `RCPT` для внешнего, своему — доставлено; `Moderated` — в очереди модерации; отказ
  модератора внешнему — без DSN.
- DSN внешнему — без внутренних имён (проверка текста по списку имён узлов и адресов группы).
- `redirect` наружу при `auto_forward = Off` — нет в очереди, письмо в ящике; домен из `auto_forward_domains` — в очереди.
