# CTR-93: контракт политики доверия — типы абонентов, функции, уровни, потолок, безопасные умолчания

lane: contracts
deps: [CTR-92]
class: B
files: [crates/tp-contracts/src/trust.rs, crates/tp-contracts/src/schedule.rs, crates/tp-contracts/src/sieve.rs, crates/tp-contracts/src/events.rs, crates/tp-contracts/src/lib.rs, docs/CONTRACTS.md, crates/tp-testkit/*, crates/tp-dav/src/deliver.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/research/2026-11-14-trust-matrix.md, docs/ADMIN-GUIDE.md#кто-с-кем-и-что-может, docs/tasks/CTR-92.md]
checks: [nextest]
manual: no

## Сделать
- Модуль `trust.rs`. `PeerKind::{Own, Partner, Neighbor, External}` — тип абонента для обоих направлений (отправитель
  входящего, получатель исходящего); `SenderKind` из CTR-92 переносится сюда под этим именем, старое имя удаляется,
  вызывающие правятся в этом же PR.
- Функции и значения (serde `snake_case`, порядок — от узкого к широкому, `Ord`):
  `invite_new` — `InviteMode::{Button, Auto}`; `itip_updates` (`REPLY`, `CANCEL` к существующим событиям), `auto_reply`,
  `mdn`, `auto_forward` — `Toggle::{Off, On}`; `freebusy`, `sharing` — `Detail::{None, FreeBusy, Limited, Details, All}`;
  `group_post` — `GroupPost::{Closed, Moderated, Open}`. `TrustMatrix` — по клетке на функцию и тип (`PerPeer<T>`);
  `TrustMatrix::default()` — таблица «Матрица по умолчанию» ADMIN-GUIDE; `TrustMatrix::closed()` — все клетки чужих в
  самом узком значении (ошибка чтения политики).
- Закреплённые клетки — не поля политики, а константы контракта `HARD_CEILING`: бронирование, GAL и поиск, делегирование
  и отправка от имени — только `Own`; прочие методы iTIP извне — нет; DSN чужим — без диагностики; `invite_new` соседу и
  интернету — не выше `Button`; `freebusy` соседу и интернету — `None`; `All` — только `Own`.
- Уровни `TrustLevel::{Installation, Org(TenantId), Group(TenantId, GroupId)}`; `TrustPolicy` уровня — частичная
  (клетки `Option`), у организации ещё `partners` и `auto_forward_domains` — имена доменов (нижний регистр, A-label IDNA).
  `TrustPolicy::validate(ceiling)` — клетки выше потолка — `Error::Invalid` с перечнем `{function, peer, value,
  ceiling, level}`; домен из принятых доменов своей организации в `partners` — `Invalid`.
- `effective(installation, org, groups) -> Effective { matrix, ceiling, source }`: потолок уровня — действующее значение
  уровня выше (у установки — `HARD_CEILING`); значение ниже потолка берётся как есть, выше — обрезается до потолка и
  помечается `over_ceiling`; из нескольких групп — самое узкое по каждой клетке; `source` — уровень, давший значение.
- `TrustStore`: `policy(level)`, `set_policy(level, &TrustPolicy)` (проверяет `validate` по действующему потолку),
  `reset_policy(level)`, `effective_for(tenant, account)` (группы учётной записи — через членство) и
  `partner_of(tenant, domain)`. Событие шины `trust_changed(level)` — сброс кэша узла.
- Решение 123: свободная функция `inbound_allowed` заменяется методом `TrustMatrix::inbound(kind, action) ->
  InboundDecision::{Apply, ApplyIfExisting, MailOnly}`; единственный вызывающий (`ItipInbox::deliver`, DAV-43) правится
  здесь же: передаёт `TrustMatrix::default()`, `ApplyIfExisting` исполняет как `MailOnly` до DAV-44.
- Sieve (`sieve.rs`): в контексте правил — `peer: PeerKind` и действующая матрица получателя; в итоге правил —
  `calendar: Option<ProcessCalendar>` (параметры RFC 9671 §3: `allowpublic`, `addresses`, `updates_only`,
  `calendar_id`, `delete_cancelled`); у `Vacation` — внешний текст: `external_subject`, `external_text_body`,
  `external_html_body` (все `None` — чужим ответа нет).
- Описание — раздел «Trust» в `docs/CONTRACTS.md` и строка CHANGELOG. `tp-testkit`: `MemTrust` и набор `suite::trust`
  — его же проходит `PgMeta` (META-85).

## Как делают другие
- Уровни с потолком сверху: Google — настройки по OU, группа важнее OU, пользователь выбирает в пределах администратора
  (https://knowledge.workspace.google.com/admin/calendar/set-google-calendar-sharing-options,
  https://workspaceupdates.googleblog.com/2026/08/new-admin-controls-for-adding-invitations-to-Google-Calendar.html);
  Zimbra — COS → учётная запись (https://zimbra.github.io/adminguide/latest/index.html).
- Партнёры отдельной сущностью: Exchange Organization Relationship и Remote Domain
  (https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/new-organizationrelationship,
  https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-remotedomain).
- При конфликте побеждает запрет — правила почтового потока Exchange
  (https://learn.microsoft.com/en-us/exchange/security-and-compliance/mail-flow-rules/mail-flow-rules).
- Отличие: типизированная политика вместо правил потока — проверка схемой и закреплённые клетки, одной строкой не
  открыть всё (исследование, «Матрица как настройка»).

## Приёмка
- Тесты `effective`: без политик — `default()`; организация поднимает `invite_new.partner` до `Auto` — действует;
  установка снижает потолок до `Button` — у организации `Auto` обрезан и помечен `over_ceiling`; две группы — самое
  узкое; `validate` отклоняет `freebusy.external = FreeBusy` и `invite_new.neighbor = Auto` с полями ошибки.
- `TrustMatrix::inbound`: `Own` — `Apply`; `REPLY` и `CANCEL` чужих — `ApplyIfExisting`; `REQUEST` партнёра — `MailOnly`
  при `Button`, `Apply` при `Auto`; бронирование чужих — `MailOnly` при любой политике.
- Набор `suite::trust` проходит `MemTrust`: уровни, сброс, чужая организация своей политики не видит, `partner_of`.
