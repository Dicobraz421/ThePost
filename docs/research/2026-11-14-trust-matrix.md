# Взаимодействие типов абонентов: как делают Exchange, Google, открытые серверы и RFC

Исследование 14.11.2026 для решения 124. Обозначения: E — Exchange / Exchange Online, G — Google Workspace; номера в скобках — источники в конце; «н/п» — первоисточник не найден.

**Итог.** Свои — только те, кто вошёл в систему, а не «наш» домен в From. Всё остальное — внешнее, пока администратор явно не расширил доступ (домен, группа, ресурс), и расширение ограничено потолком уровня выше. Соседний арендатор в облаке (EXO, G) — такой же внешний; адресные политики Exchange дают «только виртуальное разделение, не юридическое» [10].

## Что стоит по умолчанию

| Функция | Своя организация | Партнёр | Сосед по установке | Интернет |
|---|---|---|---|---|
| Почта | доставляется | E: как внешняя, фильтр не обходится, `TrustedMailInboundEnabled=$false` [2] | EXO, G: внешний; E on-prem + ABP: внутренний [10] | фильтр; NDR разрешён [2] |
| Отправка от имени, делегирование | G: делегат только своей организации [22] | нельзя [22] | нельзя | нельзя |
| DSN | `ReportToOriginatorEnabled=$true` у групп [4] | `NDREnabled=$true` [2] | как интернет | `NDREnabled=$true`; EOP молча отбрасывает сомнительное без NDR [12] |
| MDN | G: по OU, «внутри + список внешних» [23] | E: `DeliveryReportEnabled=$true` [2] | — | то же |
| Автоответ | E: внутренний текст [5] | внешний текст; MailTips через связь выключены [6] | внешний текст | `ExternalAudience=All`, отдельный `ExternalMessage` [5] |
| Письма в группы | можно | E: отклоняются, `RequireSenderAuthenticationEnabled=$true` [3]; G: выключено [21] | как интернет | отклоняются [3][21] |
| iTIP в личный календарь | авто | G: по умолчанию «от всех», с 2022 — «только известные», с 08.2026 — потолок админа по OU [17][18][19] | как интернет | E: н/п |
| Бронирование ресурсов | `AutoAccept`, `AllBookInPolicy=$true` [1] | E: `ProcessExternalMeetingMessages=$false` [1]; G: только явно добавленные адреса/домены на ресурс [20] | как интернет | отклоняется [1][20] |
| Free/busy | внутри | E: Organization Relationship, `FreeBusyAccessEnabled=$false`, уровни `AvailabilityOnly`/`LimitedDetails`, область — группа [6] | то же, если связь создана | E: Sharing Policy [8]; G: уровни [16] |
| Публикация по ссылке | — | — | — | E: домен `Anonymous` в Sharing Policy [7]; G: ограничено уровнем внешнего обмена [16] |
| Общие календари и папки | G: нет / free/busy / всё [16] | E: Simple/Detail/Reviewer для домена [7] | то же | `Anonymous` [7] |
| GAL и поиск | своя GAL | не отдаётся, фото выключены [6] | ABP: виден только свой GAL [10][11] | нет |

Открытые решения: Zimbra — «кто может меня приглашать», автодобавление приглашений, PUBLISH по умолчанию не обрабатывается, политики ресурса, автоответ раз в 7 дней, `sendToDistList` по внешнему домену, COS → аккаунт [24]. Stalwart — `autoAddInvitations=false`, лимиты размера и получателей [25]. Nextcloud, SOGo — правил для внешних не нашёл [29][30].

## Рекомендации для The Post (приняты владельцем 14.11 в части 3, 4 и способа настройки)

1. «Свой» — вошедший в систему (сессия, submission, JMAP, CalDAV). Домен в From и DMARC pass своим не делают: DMARC не проверяет local-part (RFC 9989 §2.2, §4.1). Сосед по установке — внешний [10][11].
2. Группы закрыты для внешних; админ открывает группу целиком, для доменов партнёров или с модерацией [3][4][21]. Уведомление об отказе модератора внешним выключено (источник backscatter).
3. iTIP извне: новый REQUEST — письмо с кнопкой «Добавить», в календарь сам не попадает [17][18][25], RFC 5546 §6.1.5. REPLY и CANCEL — только к существующим событиям нашего организатора, при совпадении UID и ORGANIZER/ATTENDEE (RFC 5546 §6.1.1–6.1.2, RFC 6638 §11.2). REFRESH от не-участника не обрабатывать (RFC 5546 §6.1.6). Для партнёров админ может включить автодобавление.
4. Ресурсы: внешним и соседям — отказ без ответа; бронирование извне выключено (по решению владельца не включается и для партнёров).
5. Free/busy: внешним не отдаём; партнёру — только «свободен/занят» и только для группы-области; подробности — явно [6].
6. Обмен и публикация: потолок админа по организации/группе (выключено / free/busy / детали) [7][16]; ссылка-токен с отзывом; пользователь не превышает потолок [16][17].
7. DSN: недоставку отклонять в SMTP-сессии (RFC 5321 §4.1.1.3, §6.2); если письмо принято — DSN с `MAIL FROM:<>` на адрес конверта, не на From (RFC 3464 §2); не возвращать на явно вредное (RFC 5321 §6.2); наружу — без диагностики и внутренних имён (RFC 3464 §4.2).
8. MDN: по умолчанию не отправлять; только с согласия пользователя; никогда автоматически, если `Disposition-Notification-To` ≠ `Return-Path`; не отвечать на MDN и спам (RFC 8098 §2.1, §6.1, §6.4).
9. Автоответ: внутренний и внешний тексты раздельно [5]; внешний пуст по умолчанию, и пока его нет — наружу ничего; не чаще раза в 7 дней на отправителя, только на Return-Path, не на `Auto-Submitted`, списки рассылки и пустой обратный адрес, ставить `Auto-Submitted: auto-replied` (RFC 3834 §2–§4, §3.1.7); члены группы не отвечают отправителю, писавшему в группу [4].
10. Автопересылка наружу выключена, включается для домена [2][13].
11. GAL — только свой арендатор, поиск тоже; партнёру и соседу ничего [6][10][11].

## Риски

Календарный спам и фишинг (RFC 5546 §6.1.5; [18][19]); вредоносные свойства календаря, CVE-2023-23397 [15]; подделка организатора и перехват UID (RFC 5546 §6.1.1–6.1.3, RFC 6638 §11.2, RFC 6047 §2.3); REFRESH раскрывает детали (RFC 5546 §6.1.6); backscatter (RFC 5321 §6.2, RFC 3464 §4.1, [12]); автоответ — перебор адресов, петли (RFC 3834 §6); MDN — слежка, усиление (RFC 8098 §6.2, §6.4); подделка внутреннего From в группах (RFC 9989 §4.1); утечка GAL между арендаторами [10][11]; вывод данных автопересылкой [13]; приватность free/busy (RFC 6638 §11.4); чужие `Authentication-Results` (RFC 8601 §5), подделка `remote-host` (RFC 5183).

## Матрица как настройка: способы

- E: Organization Relationship (домены партнёров: free/busy, подсказки, фото) [6]; Sharing Policy [7]; RemoteDomain (автоответ, NDR, автопересылка) [2]; Set-CalendarProcessing по ресурсу [1]; правила почтового потока — при конфликте побеждает запрет [13][14].
- G: настройки по OU, группы важнее OU [16]; для приглашений — потолок админа, пользователь выбирает в его пределах [17][19].
- Zimbra: COS → аккаунт, права `zmprov grr` [24].
- Dovecot: скрипты before → личный → after; `after` не годится для принуждения (пользователь обходит через `fileinto; stop`); `sieve_global_extensions` — расширения только глобальным скриптам [27].
- Stalwart: доверенный интерпретатор системных скриптов на этапах SMTP; при сбое письмо проходит [26].
- Cyrus: глобальные скрипты на сервер и домен, подключаются пользователем [28].
- RFC 9671 `processcalendar` (Fastmail, 10.2024): `:allowpublic :addresses :updatesonly|:calendarid :deletecancelled :organizers :outcome :reason`; не меняет PARTSTAT, не отменяет implicit keep; §5 — не обрабатывать спам, использовать списки и SPF/DKIM/DMARC. Реализация — Cyrus (`processimip`) [28]. Теста «внутренний / внешний / партнёр / сосед» в стандарте нет — нужно наше расширение. Сопутствующие: `ihave` (RFC 5463), `environment` (RFC 5183), `extlists` (RFC 6134), `vacation :addresses :days` (RFC 5230).

| | Типизированная политика | Админский Sieve | Правила почтового потока |
|---|---|---|---|
| Плюсы | проверка схемой, видно включённое, безопасные умолчания, потолок для пользователя [17] | гибкость, стандарт, тонкие исключения | знакомо админам E [14] |
| Минусы | новая потребность — новое поле | ошибку видно при выполнении; нет теста «внешний» | действуют на письмо, не на объект календаря; конфликты [13] |
| Ошибка админа | ограничена перечнем значений | одна строка открывает всё; у Stalwart сбой = пропуск [26] | условие «снаружи» легко открывает почту |

**Принято (владелец 14.11):** матрица — типизированная политика (установка → организация → группа, потолок от уровня выше, безопасные умолчания, отказ при ошибке), применяется в коде в точках решения (iTIP, автоответ, DSN, MDN, группы, free/busy, GAL) до любого Sieve. Админский Sieve (этап before, доверенный) — только для исключений внутри потолка: сузить, пометить, отклонить, `processcalendar` для партнёров; расширить права он не может; проверка при сохранении; сбой = отказ. Тип абонента вычисляет ядро и передаёт в Sieve нашим расширением; заголовкам и `remote-host` не доверять. Правила почтового потока отдельным механизмом для этой матрицы не нужны.

## Не найдено или не проверено

Как EXO добавляет внешние приглашения в календарь пользователя; ограничения Send As между организациями в E; значения по умолчанию G для внешнего обмена, MDN, автоответа; точное содержимое политики обмена E по умолчанию [9]; `processcalendar` в Dovecot и Stalwart; имена атрибутов Zimbra; внешние приглашения в SOGo и Nextcloud.

## Источники

[1] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-calendarprocessing
[2] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-remotedomain
[3] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-distributiongroup
[4] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-distributiongroup
[5] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-mailboxautoreplyconfiguration
[6] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-organizationrelationship
[7] https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-sharingpolicy
[8] https://learn.microsoft.com/en-us/exchange/sharing/sharing-policies/sharing-policies
[9] https://microsoft.github.io/Microsoft365DSC/resources/exchange/EXOSharingPolicy/
[10] https://learn.microsoft.com/en-us/exchange/address-books/address-book-policies/address-book-policies
[11] https://learn.microsoft.com/en-us/exchange/address-books/address-book-policies/turn-on-address-book-policy-routing
[12] https://learn.microsoft.com/en-us/defender-office-365/anti-spam-backscatter-about
[13] https://learn.microsoft.com/en-us/defender-office-365/outbound-spam-policies-external-email-forwarding
[14] https://learn.microsoft.com/en-us/exchange/security-and-compliance/mail-flow-rules/mail-flow-rules
[15] https://msrc.microsoft.com/blog/2023/03/microsoft-mitigates-outlook-elevation-of-privilege-vulnerability
[16] https://knowledge.workspace.google.com/admin/calendar/set-google-calendar-sharing-options
[17] https://knowledge.workspace.google.com/admin/calendar/automatically-add-events-to-calendars
[18] https://workspaceupdates.googleblog.com/2022/07/invitations-from-known-senders-only-google-calendar.html
[19] https://workspaceupdates.googleblog.com/2026/08/new-admin-controls-for-adding-invitations-to-Google-Calendar.html
[20] https://knowledge.workspace.google.com/admin/calendar/allow-non-google-users-to-book-resources
[21] https://knowledge.workspace.google.com/admin/groups/set-organization-wide-policies-for-using-groups
[22] https://knowledge.workspace.google.com/admin/gmail/let-users-delegate-access-to-a-gmail-account
[23] https://knowledge.workspace.google.com/admin/gmail/let-users-request-read-receipts
[24] https://zimbra.github.io/adminguide/latest/index.html
[25] https://www.stalw.art/docs/ref/object/calendar-scheduling/
[26] https://stalw.art/docs/sieve/interpreter/trusted/
[27] https://doc.dovecot.org/main/core/plugins/sieve.html
[28] https://www.cyrusimap.org/3.12/imap/reference/admin/sieve.html
[29] https://docs.nextcloud.com/server/latest/admin_manual/groupware/calendar.html
[30] https://wiki.sogo.nu/ResourceConfiguration

RFC: 3464, 3834, 5183, 5228, 5230, 5321, 5463, 5546, 6047, 6134, 6638, 8098, 8601, 9671, 9989 (заменил 7489) — https://www.rfc-editor.org/rfc/rfcNNNN.html
