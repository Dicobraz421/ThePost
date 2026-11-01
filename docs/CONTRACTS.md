# Контракты The Post

Межкрейтовые контракты. Код — крейт `tp-contracts`; этот файл — его спецификация. Менять только PR класса B
(сначала контракт, потом волны). Сигнатуры — ориентир для CTR-карточек; точные типы фиксирует код.
Асинхронные трейты, используемые как `dyn`, — через `async-trait`; все трейты `Send + Sync + 'static`.

## Ids
- `TenantId`, `DomainId`, `AccountId`, `MailboxId`, `MessageId`, `QueueId`, `GroupId`, `HeldId` — newtype над `Uuid` (v7), `Copy`, `Serialize`; `TenantId::DEFAULT` = `00000000-0000-0000-0000-000000000001` — организация по умолчанию.
- `Uid(u32)` — IMAP UID в ящике, монотонный; `UidValidity(u32)`.
- `ModSeq(u64)` — монотонный на ящик (IMAP CONDSTORE, состояние JMAP).
- `BlobHash([u8; 32])` — SHA-256, `Display` — hex. `Gen(u32)` — поколение объекта.
- `BlobKey { tenant, hash, generation }` → путь объекта `"{tenant}/{hash[0..2]}/{hash}/{gen}"`; `BlobKey::parse_path`.
  Поле и параметры называются `generation`: `gen` — зарезервированное слово Rust 2024.

## Errors
`enum Error { NotFound, Conflict, Invalid(String), Forbidden, QuotaExceeded, Unavailable { retry_after: Option<Duration> }, Internal(String) }`;
`type Result<T> = std::result::Result<T, Error>`. `Unavailable` — повторяемая (сеть, БД, S3).

## Tenant
SRS 3.8. Все операции хранилищ выполняются в контексте `TenantId` (параметр или контекст вызова). PostgreSQL — RLS по `tenant_id`,
параметр сеанса `app.tenant_id` через `SET LOCAL` в начале каждой транзакции. Политики (квоты, срок корзины, лимиты отправки,
ключи DKIM, профиль Milter, правило `trash.purge`) — `PolicyResolver::resolve(tenant, domain, account, mailbox?) -> Policy` по цепочке организация → домен → ящик (→ папка).

## BlobStore
SRS 3.2. Объекты неизменяемы, запись идемпотентна.
- `put(&BlobKey, Bytes) -> Result<()>`; `put_stream(&BlobKey, ByteStream, size_hint) -> Result<()>`
- `get(&BlobKey, Option<Range<u64>>) -> Result<ByteStream>`; `head(&BlobKey) -> Result<Option<u64>>`
- `delete(&BlobKey) -> Result<()>` (отсутствие — не ошибка)
- `list(prefix: &str, older_than: OffsetDateTime) -> ObjectStream` (`ObjectEntry { path, size, modified }`, `key()`) — для поиска «сирот» и миграции.
- `put_raw/get_raw/delete_raw(path)` — произвольные пути для сегментов индексов (`idx/{tenant}/{account}/...`).
  Сегменты (`*_raw`) неизменяемы: путь записывается один раз, перезапись запрещена (новая версия — новый путь, манифест
  индекса переключается CAS); кэш BLOB кэширует их без инвалидации (решение 2026-10-30, #35).

## BlobIndex
SRS 3.4. Учёт объектов в метаданных. Реализация — `tp-meta`; in-memory — `tp-testkit`.
- `acquire(tenant, hash, refs: u32) -> Result<Acquire>`; `Acquire::Existing { generation }` — ссылки добавлены,
  загрузка не нужна; `Acquire::Absent { generation }` — загрузить по нему и вызвать `commit_new`.
- `commit_new(tenant, hash, generation, size, refs) -> Result<Commit>`; `Commit::Done { generation }` (письмо ссылается
  на это поколение) или `Commit::Bumped { generation }` — запись была `deleting`: загрузить по новому `gen` и повторить `commit_new`.
- `release(tenant, hash, generation, refs) -> Result<()>` — при обнулении фиксирует `zero_since`.
- `gc_candidates(older_than, limit) -> Result<Vec<BlobKey>>`; `mark_deleting(&BlobKey, older_than) -> Result<bool>` (перепроверяет окно W);
  `finalize_delete(&BlobKey) -> Result<bool>` — заменяет запись надгробием, только если `gen` не изменился.
  Надгробие хранит поколение удалённого объекта: загрузка, начатая до удаления, получит `Bumped`; удаляется по истечении
  срока, большего максимальной длительности загрузки.
- `exists(&BlobKey) -> Result<bool>` — для поиска «сирот».
- Сжатие (SRS 3.2, CTR-21; код — `blob.rs`): `Compression { None, Zstd }` (строки `none`/`zstd`) — признак в записи учёта; хэш и `size` —
  от исходного содержимого. Писатель сжимает zstd, если это уменьшает размер и формат не сжат (архивы, изображения, видео), и только
  при `supports_compression()`; запись — `commit_new_with(.., compression)` (при `Done` с поколением параллельной записи признак — её;
  после `Bumped` — тот же признак), чтение — `compression(&BlobKey) -> Option<Compression>` (`None` — записи нет, как `exists`).
  Методы по умолчанию: `supports_compression = false`, `commit_new_with(None)` = `commit_new`, иное — `Internal`, `compression` —
  `None`-сжатие для существующей записи; `commit_new` = `commit_new_with(.., None)`. Реализация признака — `tp-meta` (META-24),
  in-memory — `MemBlobIndex`; набор — `suite::blob_compression`.

## Metadata
SRS 3.1, 3.6. Все операции ящика принимают `AccountScope { tenant, account }`; без связей между ящиками.
`ModSeq` выдаётся из одного счётчика учётной записи: монотонен в папке (CONDSTORE) и в учётной записи (состояние JMAP).
- `AccountStore`: `get`, `by_login`, `upsert` (из LDAP), `quota`.
- `MailboxStore`: `list(account)`, `get`, `create`, `rename`, `delete(scope, id, cascade)`, `set_subscribed`, `status` (counts, uidnext, highest modseq).
  `delete` папки с вложенными без `cascade` — `Conflict`: решение («удалить вместе с вложенными?») принимает пользователь
  в клиенте, клиент повторяет вызов с `cascade = true`; письма всего поддерева — в корзину.
- `MessageStore`: `insert(scope, mailbox, NewMessage) -> Inserted { id, uid, modseq }` (APPEND, импорт);
  `insert_batch(tenant, Vec<(AccountScope, MailboxId, NewMessage)>) -> Vec<Inserted>` — все доставки одного приёма
  одной транзакцией: вставлены все или ни одного, события доставки — в ней же (решение 2026-10-31-atomic-delivery); `get`, `list(mailbox, MessageQuery, limit)`,
  `set_flags(ids, FlagOp, unchanged_since) -> FlagsUpdate`, `copy`, `move_to`, `to_trash`, `restore`, `expunge_final`.
  `copy`/`move_to` создают новые `MessageId`; `to_trash`/`restore` сохраняют `MessageId` (меняются папка и UID);
  `expunge_final` удаляет только письма в корзине (остальные пропускает).
- `NewMessage`: `skeleton: BlobKey`, `parts: Vec<PartRef>`, `size`, `received_at`, `flags`, `meta: MessageMeta`
  (envelope, bodystructure, preview, thread key — вычисляются при приёме).

## ChangeLog
Журнал изменений для IMAP QRESYNC и JMAP `/changes`: `message_changes(scope, mailbox?, since: ModSeq, limit)
-> Changes<MessageChange>`, `mailbox_changes(scope, since, limit) -> Changes<MailboxId>`;
`Changes { created, updated, destroyed, new_state: ModSeq, has_more }`; устаревший `since` — `Conflict`. Пишется в той же транзакции, что изменение.

## MailStore
Высокоуровневый API, крейт `tp-mailstore` (полоса mailstore). Протоколы (smtp, imap, jmap, admin) работают через него.
- Модель «скелет + части»: письмо = объект-скелет (заголовки, границы MIME, ссылки) + объекты частей.
  Побайтовая реконструкция исходного письма обязательна.
- `ingest(IngestRequest { tenant, mail_from, raw: ByteStream, deliveries: Vec<Delivery { account, mailbox, extra_headers, flags }>, received_at })
  -> Result<Ingested { delivered: Vec<Delivered { account, mailbox, message, uid }>, forward: Vec<String> }>` — одна загрузка частей
  на всех получателей; квота одного получателя — `QuotaExceeded` на весь запрос. `mail_from` — отправитель конверта (SMTP `MAIL FROM`,
  пустой — уведомление о недоставке), из заголовков не выводится. `forward` — внешние адреса `CopyTo::Address` от точек принятия
  решений: в очередь исходящей почты их ставит вызывающий (smtp, jmap), копии `CopyTo::Mailbox` делает хранилище.
- `fetch_raw(AccountScope, message, Option<Range>) -> ByteStream`; `fetch_part(AccountScope, message, part_path)` — тело в кодировке передачи.
- `set_flags`, `copy`, `move_to`, `delete_to_trash`, `restore` (по `AccountScope`) — публикуют событие в `EventBus` после фиксации.

## EventBus
SRS 2.1. `publish(Event)`, `subscribe(AccountScope) -> EventStream`, `subscribe_system()`. `Event { tenant, account: Option, kind }`,
`kind: MailboxChanged { mailbox, modseq } | MailboxListChanged | QueueReady | ConfigReloaded`.
Реализации: в процессе (`tp-cluster`), PostgreSQL LISTEN/NOTIFY (`tp-meta`, кластер; решение 2026-11-01, SRS 2.8). Доставка — at-most-once;
клиент после переподключения сверяется по `ModSeq`. Публикация — только после фиксации, вне транзакции изменения. CTR-23: `kind` дополняется
`LogAppended { partition }`, `MembersChanged`, `Blocked { subject }`, `Resync` (переполнение буфера подписчика — сверка с хранилищем);
`subscribe_system()` получает системные виды. Valkey удаляется (CLU-09).

## Lease
`LeaseStore`: `try_acquire(name, holder, ttl) -> Option<Lease>`, `renew(&Lease, ttl) -> Option<Lease>`, `release`.
`Lease { name, holder, fencing, expires_at }`; `fencing` растёт при каждом новом захвате. Лидер maintenance,
индексатор ящика, записи очереди. Реализация — PostgreSQL.

## Events
SRS 2.5. Журнал событий ядра (outbox): событие пишет реализация хранилища метаданных (`tp-meta`) в той же транзакции, что изменение;
метода записи в `EventLog` нет.
`CoreEvent { tenant, account, mailbox?, kind, at }`, `kind: Delivered | Submitted | FlagsChanged | Moved | Trashed | Purged | MailboxCreated | MailboxDeleted`.
Раздел = `partition_of(account, N)` (FNV-1a, зафиксирован тестом); подписчик (`EventSink`: индексатор, модули) читает
`read(partition, after, limit)`, позиция — `position(subscriber, partition)`, фиксирует `commit(subscriber, partition, pos)`; раздел подписчика — под лизой. Доставка не менее одного раза, порядок — в пределах ящика.
Между экземплярами (решение 2026-10-30, #45): `PartitionAssigner` (`tp-cluster`, rendezvous-хэширование `<подписчик>/<раздел>`
по `Membership::live()`) — `assigned(subscriber, &NodeId, partitions) -> Vec<Partition>` (по возрастанию), `members() ->
watch::Receiver<Vec<NodeId>>` (новое значение — пересчитать). Для одного состава каждый раздел назначен ровно одному узлу; узел вне
состава — пусто, единственный узел — все. Узел берёт лизу только назначенного ему раздела и отдаёт лизы снятых разделов после
фиксации текущей пачки; лиза с `fencing` остаётся гарантией единственности, пока узлы видят разный состав.

## Membership
Решение 2026-10-30, #45. `Membership`: `heartbeat(&NodeId, ttl)` — создать или продлить запись узла (период ≈ `ttl / 3`),
`leave(&NodeId)` — удалить (штатная остановка; отсутствующая — не ошибка), `live() -> Vec<NodeId>` — узлы с непросроченной записью,
по возрастанию. Время — часы хранилища (PostgreSQL). `NodeId` — имя экземпляра, не пустое, без пробелов, ≤ 255 байт (строка в JSON).
Реализация — `tp-meta` (META-16), in-memory — `tp-testkit`. Код — `membership.rs`.
CTR-23: запись узла дополняется ролями и адресом служебного канала (`NodeInfo { id, roles, addr }`, `live_info()`); вход и выход узла
публикуются сигналом `MembersChanged`; `PartitionAssigner::members()` обновляется по сигналу, а не опросом.

## Rpc
Решение 2026-11-01, SRS 2.8 (CTR-23). Запрос узел → узел: `Rpc::call(service, key, body, deadline) -> Result<Bytes>`; `service` — роль и имя
службы, маршрут — `live_info()` с нужной ролью, владелец ключа — rendezvous-хэширование, при отказе — следующий. Ошибки: `Busy` (параллельность
превышена), `Timeout`, `Unavailable`. Обработчик — `RpcHandler` регистрируется ролью; матрица разрешений «роль → службы» проверяется
на принимающем узле по сертификату вызывающего. Транспорт — HTTPS/2 с mTLS (`InternalChannel::Node`). Реализация — `tp-cluster`.

## AbuseBlocks
Решение 2026-11-01, SRS 6.3 (CTR-23). Блокировки источников и учётных записей: `block(tenant, subject, until, reason)`,
`blocked(tenant, subject) -> Option<until>`; `subject` — IP, подсеть или учётная запись. Счётчики попыток — на узле (`tp-auth`);
факт блокировки — в PostgreSQL (`tp-meta`) и сигнал `Blocked`.

## DeliveryLog
SRS 5.2, решение 2026-11-01-borrowings-security (CTR-21; код — `delivery.rs`). Журнал доставки (message tracking): только добавление
и поиск, содержимое писем не хранится. `DeliveryEvent { tenant, at, node: NodeId, kind, sender, recipients, message_id: Option<_>
(без угловых скобок), queue_id: Option<QueueId>, decision, reason: Option<String> }`; `DeliveryEventKind { Receive, Reject, Deliver,
Relay, Dsn }`, `DeliveryDecision { Accept, Reject, Defer, Quarantine, Discard, Hold }` (строки в нижнем регистре).
`DeliveryLog`: `record(event)`, `record_batch(Vec<_>)` (по умолчанию — по одной), `search(tenant, &DeliveryQuery) -> DeliveryPage
{ events, next: Option<DeliveryCursor> }`, `purge(before)` — по всем организациям (maintenance), можно целыми сутками-секциями.
`DeliveryQuery { sender, recipient, message_id, since, until, limit, after }`: условия по «и», адреса — точно без учёта регистра ASCII,
Message-ID — точно, интервал `[since, until)`; `validate()` (`limit ≥ 1`, непустой интервал) и `matches(&event)` — в контракте;
порядок — по `at`, равные — в порядке записи; некорректный запрос или позиция — `Invalid`. Срок хранения (по умолчанию 30 суток) —
секция конфигурации реализации. Пишет `tp-smtp` (SMTP-17); реализация — `tp-meta` (META-22: секции по суткам, роль приложения —
только INSERT и SELECT), in-memory — `MemDeliveryLog` (`events()`); набор — `suite::delivery_log`.

## Groups
SRS 4.5, решение 2026-11-01-borrowings-security (CTR-21; код — `groups.rs`). `DistributionGroup { id: GroupId, tenant, name, address:
Option<_> (уникален без учёта регистра; `None` — только вложенная), external_id: Option<_> (каталог, уникален), members:
Vec<GroupMember::{Account, Group}>, senders: SenderRestriction::{Anyone, Internal, Only { accounts, groups }}, moderators:
Vec<AccountId> (непусто — модерируемая), hidden_from_gal }`; `validate()`, `moderated()`, `requires_moderation(sender)` (модераторы —
без одобрения). `GroupStore`: `get`, `by_address`, `by_external_id`, `list(tenant)` (по `id`), `put(group)` — замена целиком
(`Invalid`; занятый адрес или `external_id` — `Conflict`), `remove` — возвращает письма очереди группы (отсутствующая — пусто),
`expand(tenant, id) -> Vec<AccountId>` — с вложенными, каждая группа один раз (циклы), по возрастанию без повторов, неизвестные
вложенные группы пропускаются, нет корня — `NotFound`; `sender_allowed(tenant, &group, Option<AccountId>)` — по умолчанию через
`expand` (`None` — внешний). Ограничения и модерация — для группы-адресата, не для вложенных. Очередь модерации:
`HeldMessage { id: HeldId, group, sender, sender_account, message_id, blob, size, held_at }` (ссылка на объект — у вызывающего);
`hold` (нет группы — `NotFound`, занятый id — `Conflict`), `held(tenant, Option<GroupId>, limit)` по (`held_at`, `id`),
`take_held(tenant, id)` — забрать для решения (повторно — `NotFound`). Синхронизация из каталога — `tp-auth` (AUTH-07), проверки
при приёме — `tp-smtp` (SMTP-16), Admin API — ADM-07; реализация — `tp-meta` (META-21), in-memory — `MemGroups`; набор — `suite::group_store`.

## Search
SRS 4.6. `SearchIndex`: `query(AccountScope, mailboxes: &[MailboxId] (пусто — все), &TextQuery, SearchSort, limit) -> Vec<MessageId>`,
`snippet(AccountScope, message, &TextQuery) -> Snippet { subject?, preview? }` (`<mark>`). `TextQuery` — дерево
`Term { field: TextField, text } | And | Or | Not`; нетекстовые условия фильтрует вызывающий по метаданным.
`IndexManifestStore` (метаданные): `get_manifest(AccountScope) -> IndexManifest { version, schema, segments: Vec<IndexSegment { path, size, docs }>,
rebuild: Option<RebuildRequest { requested_at, reason: RebuildReason::{ Admin, Corrupt, Restore } }> }` (`size()`, `docs()` — суммы),
`put_manifest(AccountScope, expected, ManifestUpdate { schema, segments, rebuilt }) -> version` (несовпадение версии — `Conflict`;
`rebuilt` снимает `rebuild`). Перестройка: `request_rebuild(scope, request) -> version` (версия растёт — идущая перестройка
получит `Conflict` и начнётся заново), `pending_rebuilds(schema, limit) -> Vec<AccountScope>` — запрошенные и со схемой старше
`schema` (все организации); новый индекс строится в новых путях, прежний обслуживает запросы до переключения.

## Directory
SRS 5.1, 3.8. Каталог организации: `accounts_since(tenant, Option<&DirCursor>, limit)`, `groups_since` — инкрементально,
`DirPage { changes: Vec<DirChange::{Upsert, Removed}>, next, has_more }`, устаревший курсор — `Conflict`;
`aliases`, `domains`, `gal_attributes` (имена атрибутов GAL; значения — `DirAccount::attributes`) — полным списком.
Реализация — `tp-auth` (LDAP); модуль «Мультитенантность» подключает каталог на организацию. Код — `auth.rs`.

## Auth
SRS 5.1. `Authenticator`: `verify(Credentials) -> Result<Principal>` (неверные данные — `Forbidden`);
`Credentials::{ Password { login, secret }, Bearer(token), Gssapi { principal, authzid } }`, секреты скрыты в `Debug`.
`Principal { tenant, account, login, groups, scopes: Option<_> }` (`None` — без ограничений).
`Authorizer`: `rights(&Principal, &Resource) -> Rights`, `check(.., required)` (по умолчанию через `rights`),
`send_right(&Principal, from) -> Option<SendRight::{SendAs, SendOnBehalf}>`; `Resource::{ Account, Mailbox }`.
`Rights` — RFC 4314, строка `lrswipkxtea` в каноническом порядке; `c`/`d` (RFC 2086) разворачивает IMAP.
SASL (код — `sasl.rs`, RFC 4422): механизмы `Mechanism::{ Plain, Login, OauthBearer, Xoauth2, Gssapi }` (`name()`, разбор без учёта регистра,
`plaintext()` — без TLS не предлагается) — `tp-auth` поверх `Authenticator`; `SaslServer { mechanisms(tls) -> Vec<Mechanism>,
start(mech, &SaslContext { service, tls }) }` → `SaslSession::step(Option<&[u8]>) -> SaslStep::{ Challenge(bytes), Done(Principal), Fail(SaslFailure) }`
(`None` — нет начального ответа; `SaslFailure::{ Authentication, Authorization, Expired }` — коды RFC 5530 / SMTP 535).
Неизвестный или недоступный механизм, нарушение формата — `Invalid`; base64 и отмена `*` — протокол.
ACL (код — `acl.rs`, SRS 4.5, RFC 4314): `AclStore` — данные `Authorizer`, IMAP ACL, JMAP и Admin API. Идентификаторы — имя входа, группа,
`ANYONE` (`"anyone"`), с `-` в начале — отрицательные права; `validate_identifier(id, negative)` (≤ 255 байт, без пробелов и управляющих, не `-`)
→ `Invalid`. Права владельца на свои папки — правило `Authorizer`, в ACL не хранятся.
`acl(owner: AccountScope, mailbox) -> Vec<AclEntry { identifier, rights }>` (по идентификатору; нет папки — `NotFound`);
`set_acl(owner, mailbox, identifier, rights)` — замена целиком, пустые права удаляют запись; удаление папки удаляет её ACL;
`acl_rights(owner, mailbox, identifiers) -> Rights` — записи субъекта и `anyone` минус отрицательные (`effective_rights`), нет папки — `NONE`;
`shared_mailboxes(tenant, identifiers) -> Vec<SharedMailbox { account, mailbox, rights }>` — папки с правом `l`, по (`account`, `mailbox`).
Делегирование: `send_delegations(owner) -> Vec<SendDelegation { delegate, right }>`, `set_send_delegation(owner, delegate, Option<SendRight>)`
(`None` — отзыв; нет учётной записи — `NotFound`), `send_right_of(owner, identifiers) -> Option<SendRight>` — сильнейшее (Send As), без `anyone`.
Пароли приложений (код — `app_password.rs`): `AppPasswordStore` — хранилище для `tp-auth` (выпуск, проверка argon2id и предел — там);
`AppPassword { id, label, hash, created_at, revoked_at: Option<_> }` (хэш — PHC-строка, скрыт в `Debug`; `validate()`: id 1–64 байт без пробелов
и управляющих, label ≤ 255 символов без управляющих, hash непустой → `Invalid`). `list(scope)` — включая отозванные, по (`created_at`, `id`);
`insert(scope, AppPassword)` — нет учётной записи — `NotFound`, занятый id — `Conflict`; `revoke(scope, id, at)` — нет — `NotFound`,
повторный отзыв сохраняет первый момент.

## Roles
SRS 2.1. `enum Role { SmtpIn, Submission, SmtpOut, Imap, Jmap, Dav, Admin, Search, Indexer, Maintenance }`; пресеты (`all`, `edge`, `transport`, `access`, `backend`, `mailbox`) раскрываются в набор ролей при загрузке конфигурации; режим edge — параметр smtp-in (SRS 2.6).
Раскрытие — `RoleSet::expand(&[RoleEntry]) -> RoleSet { roles, edge }`; `RoleEntry` = пресет | роль (строка config.yaml).
`RoleService`: `name()`, `run(ctx: RoleCtx) -> Result<()>` до отмены `shutdown`; `RoleCtx { config: ConfigRx,
shutdown: CancellationToken, drain_deadline, ready: ReadyHandle }` (`tokio`, `tokio-util`; `role` реэкспортирует `watch` и `CancellationToken`).
`ConfigRx = watch::Receiver<Arc<dyn Any + Send + Sync>>` — секция роли; типизированно — `RoleCtx::config::<T>()`, ожидание версии —
`RoleCtx::config_changed()` (закрытый канал — не завершается). `config_channel(v)`, `fixed_config(v)` — для приложения и тестов; метрики — позже.
`RoleCtx::load: Arc<dyn LoadMonitor>` — уровень нагрузки узла (раздел Load), `RoleCtx::load_level()`; до PLT-15 приложение передаёт
`fixed_load(LoadLevel::Normal)`.

## Load
SRS 6.3, решение 2026-11-01-borrowings-security (CTR-21; код — `load.rs`). `LoadLevel { Normal, Elevated, High }` (по возрастанию,
строки `normal`/`elevated`/`high`): `Elevated` (по умолчанию 85 %) — `pauses_background()`: фоновые задания (индексация, GC, миграция)
приостановлены, меньше параллельность новых сессий, замедление подозрительных источников; `High` (95 %) — `limits_new_sessions()`:
SMTP — 4xx новым сессиям, IMAP и JMAP — ограничение новых сессий, принятые письма обрабатываются.
`LoadMonitor`: `subscribe() -> LoadRx` (`watch::Receiver<LoadLevel>`: новое значение — переход между уровнями), `level()` (по умолчанию
через `subscribe`). Показатели, пороги относительно лимитов cgroups, гистерезис, метрики — реализация `tp-platform` (PLT-15);
`fixed_load(level)` — постоянный уровень (канал открыт); подменный — `tp_testkit::MemLoadMonitor` (`set(level)`).

## Config
Каждый крейт владеет своей секцией `XxxConfig: Deserialize + Default + Clone` и документирует её поля.
Приложение собирает общий `Config`; обязательны только `database.url`, `blob.url`, `domain` (SRS 5.5).
Hot reload: новая копия рассылается через `watch`; параметры, требующие перезапуска, помечены в doc-комментарии.
Длительности (решение 2026-10-30, #43) — только строки `5s`, `250ms`, `10m`, `24h`, `1h30m` (единицы `d h m s ms us ns` по убыванию,
каждая раз, без пробелов; ноль — `0s`); поля без единицы в имени (`acquire_timeout`, `reload.poll_interval`). Поле —
`#[serde(with = "tp_contracts::config::duration")]` + `#[schemars(with = "duration::HumanDuration")]` (`Option` — `duration::option`
и `Option<HumanDuration>`); схема — `type: string`, `pattern` = `duration::PATTERN`. Число, `5 sec`, `-1s`, переполнение — ошибка.
Секреты — только `secrets::SecretRef` (раздел Secrets); строка или число на месте ссылки — ошибка разбора.

## Secrets
Решение 2026-10-30. Секреты (пароли PostgreSQL, ключи S3, DKIM, ACME, подписи токенов, LDAP/OIDC) — только во внешнем хранилище
(OpenBao / Vault, API Vault). В config.yaml и таблицах БД — `SecretRef`: в конфигурации `{ secret: "<путь>#<поле>" }`
(`deny_unknown_fields`), в БД — строка `<путь>#<поле>` (`Display`/`FromStr`); путь — сегменты `[A-Za-z0-9_.-]+` через `/` без `.`/`..`,
поле — `[A-Za-z0-9_.-]+`. Скаляр на месте ссылки — ошибка «secret in plaintext» без значения в тексте. `Secret` — значение,
`***` в `Debug`/`Display`, обнуляется при удалении, доступ — `expose()`; `lease() -> Option<&SecretLease { id, ttl, renewable }>`,
`lease_ttl()`. `SecretStore`: `get(&SecretRef) -> Result<Secret>` (`NotFound` — нет пути или поля, `Forbidden` — политика,
`Unavailable` — недоступно или запечатано), `renew(&SecretLease) -> Result<SecretLease>` (неизвестная/истёкшая — `NotFound`, перечитать;
по умолчанию — `NotFound`). Встроенный клиент — `tp-platform` (PLT-12), замена — `Registry::set_secret_store`, in-memory — `tp-testkit`.
Код — `secrets.rs`.

## Internal TLS
SRS 6.2, решение 2026-10-30. Узел ↔ PostgreSQL, S3, узел ↔ узел — только TLS. Общая секция `tls::InternalTls
{ ca_file, cert_file, key_file, server_name, allow_plaintext }` — поле `tls` секции канала; `Profile { Prod (по умолчанию), Dev }` —
параметр `profile` верхнего уровня config.yaml. `InternalTls::check_url(InternalChannel::{Postgres, S3, Node}, url, profile)
-> Result<Transport::{Tls, Plaintext}>` вызывается в `thepost config check` и при запуске: `allow_plaintext` вне dev, адрес без TLS
без `allow_plaintext`, `sslmode=require|verify-ca` — `Invalid`. Схемы: PostgreSQL — `sslmode=verify-full` (без `sslmode` клиент
обязан verify-full), S3 — `https://`, узел — `https://`/`smtps://`. `Transport::Plaintext` — крейт канала пишет
`InternalChannel::plaintext_warning()` в лог на каждом запуске. Служебный HTTP — TLS слушателя (`tp-platform`).

## Transport (Ф1)
SRS 4.1, 2.6. Код — `transport.rs` (CTR-24). Настройки транспорта — данные в PostgreSQL; узлы держат снимок в памяти и перечитывают его
по сигналу `TransportChanged { tenant: Option<TenantId> }` (`None` — уровень кластера) и при `Resync`.
`TransportStore`: `cluster() -> ClusterTransport { version, domains: Vec<AcceptedDomain { id: DomainId, tenant, name, kind: DomainKind::{
Authoritative, InternalRelay { next_hop } } }>, sources: Vec<TrustedSource { name, matcher: SourceMatch::{ Networks(Vec<IpNet>),
ClientCert { subject } }, kind: SourceKind::{ Gateway { xclient, authserv_id: Option<String> }, Application { senders: Vec<AddressPattern>,
external: bool } } }> }` — для выбора организации и источника до аутентификации; `tenant(tenant) -> TenantTransport { version,
routes: Vec<DestinationPolicy { pattern ("*" — все внешние), tls: TlsRequirement, smart_host?, egress_ip? }>, rules: Vec<FlowRule> }`.
Запись — `put_*`/`delete_*` с проверкой `version` (устаревшая — `Conflict`), публикация сигнала — после фиксации.
`FlowRule { id, priority, enabled, mode: RuleMode::{ Enforce, Audit }, conditions: Vec<Condition>, exceptions: Vec<Condition>,
actions: Vec<Action>, stop: bool }`; `Condition` — отправитель или получатель (адрес, домен, группа, `Internal`/`External`), заголовок,
тема и текст (слова, регулярное выражение), вложение (тип, имя, размер), размер письма; `Action::{ Reject { text }, Drop, AddHeader,
RemoveHeader, PrependSubject, Disclaimer { text, html }, Copy(address), Redirect(address), Moderate, Route(pattern) }`.
`MailFlow` (реализация — `tp-smtp`): `apply(&FlowInput { tenant, direction: Inbound | Submission, envelope, message: BlobRef })
-> FlowOutcome { verdict: Accept | Reject(text) | Drop | Moderate, envelope, message: Option<BlobRef>, route?, matched: Vec<RuleId> }`
— один раз на письмо, до `DecisionHooks`; изменённое тело — новый объект BLOB (ссылку учитывает вызывающий); `Audit` — только `matched`.

## Queue (Ф1)
SRS 4.1. Код — `queue.rs`. `QueueStore` (общая для организаций; время передаёт вызывающий):
`enqueue(OutMessage { tenant, blob, size, from, dsn: DsnParams { envid, ret }, rcpts: Vec<OutRcpt { address, notify, orcpt }>, next_attempt }) -> QueueId`
(пусто или повтор адреса — `Invalid`); `claim(now, limit, lease) -> Vec<Claimed { claim: QueueClaim { id, fencing }, until, queued_at, .., rcpts: Vec<QueuedRcpt { rcpt, attempts, last_error }> }>`
— записи с наступившим сроком попытки без действующей лизы; `renew(&claim, until)`, `reschedule(&claim, rcpt, at, reason)` (`attempts` + 1),
`complete(&claim, rcpt, RcptStatus::{ Delivered { reply }, Failed { reply } }) -> bool` (`true` — все получатели завершены, запись удалена).
Устаревший захват — `Conflict`; не завершённые до истечения лизы получатели возвращаются в очередь без `attempts` + 1.
Тело — один объект BLOB: ссылку учитывает вызывающий до `enqueue` и освобождает после `complete` = `true`.
Расписание повторов, срок доставки и DSN — `tp-smtp` (DSN ставится в очередь до `complete(.., Failed)`).

## Trash, Retention, Quota (Ф1)
SRS 4.7, 3.8. Код — `retention.rs`. `TrashStore`: `list(scope, offset, limit) -> Vec<TrashItem { scope, object: TrashObject::Message(id), origin, size,
received_at, deleted_at, expires_at }>`, `restore(scope, object) -> Restored::Message { mailbox, inserted }`,
`expired(now, limit)` (все организации, по `expires_at`, включая скрытые), `defer(scope, object, until)`.
«Очистить корзину» (решение 2026-11-01-trash-purge): `purge(scope, ids, &DecisionHooks) -> PurgeOutcome { hidden, erased, modseq }` —
письма, видимые в корзине, по настройке `trash.purge`: `TrashPurge::Hide` (по умолчанию) — скрыть, `Erase` — для каждого
`before_final_delete` вне транзакции: `Allow` — стереть (как `expunge_final`), `Deny` (Legal Hold) или ошибка хука — скрыть;
прочие письма — пропуск. Скрытое письмо не видно пользователю: нет в `MessageStore::get`/`list` и операциях над письмами,
в счётчиках папки, в квоте (`Quota`, `AccountStore::quota`); `ChangeLog` — удалено из корзины, событие ядра — `Trashed`.
Администратор: `list_hidden(scope, offset, limit)`, `restore` (видимые и скрытые; исходная папка или «Входящие», прежний `MessageId`);
`list` — только видимые. Срок корзины для скрытых — как для остальных (`expired` → хук → `expunge_final`).
Настройка: `Policy::trash_purge: Option<TrashPurgeRule { mode, user_can_choose }>` (администратор, уровни цепочки; не задано —
`hide` без выбора пользователя); `PolicyResolver::trash_purge(scope) -> TrashPurgeSetting { rule, user }`,
`effective()` — выбор пользователя только при `user_can_choose`, иначе значение администратора;
`set_user_trash_purge(scope, Option<TrashPurge>)` — без `user_can_choose` выбор `Forbidden`, сброс (`None`) разрешён.
Срок корзины — `PolicyResolver` (`trash_retention`, по умолчанию 30 суток из конфигурации): хранилище вычисляет `expires_at`
при переносе в корзину; изменение политики действует на элементы, удалённые после него.
Истечение: `expired` → `DecisionHooks::before_final_delete` (`FinalDelete::mailbox` = `origin`) → `MessageStore::expunge_final`; `Deny` — `defer`.
`expunge_final` удаляет только письма, которые всё ещё в корзине; восстановленные между `expired` и `expunge_final` и прочие — пропуск, не ошибка.
`Quota`: `usage(scope) -> QuotaUsage` (письма, включая корзину, без скрытых; лимит — `PolicyResolver::quota_bytes`), `check(scope, bytes)` → `QuotaExceeded`
(по умолчанию через `usage` и `QuotaUsage::allows`).
Политики хранения и Legal Hold — модуль «Соответствие требованиям» через `DecisionHook::before_final_delete`.

## Extension points
SRS 2.5, 1.3. Модуль — cargo feature + `modules.<имя>.enabled`. `Module { name(), schema() -> "mod_<имя>", migrations() -> &[Migration { version, name, sql }],
register(&mut Registry, &ModuleCtx) -> Result<()> }`; имя — `validate_module_name` (`[a-z][a-z0-9_]*`, ≤ 59).
`ModuleCtx { config: watch::Receiver<serde_json::Value> }` (`#[non_exhaustive]`, `ModuleCtx::new`) — секция `modules.<имя>`;
типизированно — `ModuleCtx::config::<T: DeserializeOwned>() -> Result<T>` (ошибка полей — `Invalid`).
`Registry`: `add_role(RoleService)`, `add_jmap_capability(JmapCapability { uri, session_object, methods, call })`,
`add_admin_routes(AdminRoutes { prefix, handle(AdminRequest) -> AdminResponse })`, `add_event_sink`, `add_decision_hook`;
`set_blob_store` / `set_directory` / `set_search_index` / `set_secret_store` — замена встроенной реализации. Повтор имени, URI, префикса, реализации — `Conflict`.
JSON в JMAP (`session_object`, аргументы и результат `call`) и тела Admin API модулей — `serde_json::Value`
(`Null` — тела нет; ответ — `application/json`).
`DecisionHook { name, before_final_delete(&FinalDelete), on_delivery(&DeliveryInfo), on_submission(&SubmissionInfo) }`, методы по умолчанию — `Allow`;
→ `Decision::{ Allow, Deny(reason), Augment { headers, copies: Vec<CopyTo::{ Mailbox(Delivery), Address }> } }`.
`DecisionHooks` вызывает хуки по порядку регистрации: первый `Deny` прерывает, дополнения объединяются, без хуков — `Allow`;
для удаления `Augment` = `Allow`. Ошибка хука — ошибка операции. `on_delivery` вызывает `MailStore::ingest` до загрузки,
`on_submission` — submission и JMAP EmailSubmission до очереди. Данные модуля — только в схеме `mod_<имя>` с собственными миграциями.

## CHANGELOG
- 2026-10-29 — первая редакция (по SRS ред. 3).
- 2026-10-30 — ред. 4: Tenant, Events (журнал событий вместо Outbox), SearchIndex, Directory, Extension points; Legal Hold — модуль.
- 2026-10-30 — CTR-01…04 (волна contracts-01): `generation` вместо `gen`, `Commit::Done { generation }`, `mark_deleting(.., older_than)`,
  `ObjectEntry`, `AccountScope`, `ModSeq` учётной записи, `Changes<T>`, системная подписка шины, fencing лиз, `EventLog` без `append`,
  `RoleSet`/`RoleEntry`, `ConfigWatch`/`Shutdown` вместо tokio-типов.
- 2026-10-30 — CTR-05, CTR-10 (волна contracts-02): `MailStore` по `AccountScope`, `SearchSort`/`Snippet`, CAS манифеста индекса,
  `DirPage`/`DirChange`, `Authorizer::rights`/`send_right`, `SaslServer`/`SaslSession`; `ext`: `Migration`, `JmapCapability`, `AdminRoutes`,
  `DecisionHooks`, `CopyTo`.
- 2026-10-30 — x/mailbox-delete-cascade: `MailboxStore::delete(.., cascade)`; надгробия BLOB в `finalize_delete`;
  `to_trash`/`restore` сохраняют `MessageId`.
- 2026-10-30 — CTR-11 (волна contracts-04): `tokio`/`tokio-util`/`serde_json` в контрактах; `RoleCtx { config: ConfigRx, shutdown: CancellationToken }`
  вместо `ConfigWatch`/`Shutdown`; `ModuleCtx::config::<T>()`; `serde_json::Value` в `JmapCapability` и `AdminRequest`/`AdminResponse`
  (`content_type` убран).
- 2026-10-30 — CTR-12 (волна contracts-04): `tls::InternalTls`, `Profile`, `InternalChannel`, `Transport`; сегменты `*_raw` неизменяемы.
- 2026-10-30 — CTR-13 (волна contracts-05): `config::duration` (длительности humantime); `reload.poll_interval_ms` → `reload.poll_interval` (tp-platform).
- 2026-10-30 — CTR-14: `membership::{Membership, NodeId}`, `events::PartitionAssigner`.
- 2026-10-30 — CTR-15: `secrets::{SecretRef, Secret, SecretLease, SecretStore}`, `Registry::set_secret_store`.
- 2026-10-31 — CTR-16 (решение 2026-10-31-ingest-envelope): `IngestRequest::mail_from`; `ingest` → `Ingested { delivered, forward }`.
- 2026-10-31 — CTR-06 (волна contracts-03): `queue::QueueStore` с захватом записей (`QueueClaim`, fencing), `OutMessage` с параметрами DSN,
  `RcptStatus`, `QueueId`.
- 2026-10-31 — CTR-07 (волна contracts-03): `retention::{TrashStore, TrashItem, TrashObject, Restored, Quota}`, `QuotaUsage::allows`;
  раздел «Trash, Quota» → «Trash, Retention, Quota».
- 2026-10-31 — CTR-08 (волна contracts-03): SASL из `auth` в `sasl`; `Mechanism`, `SaslContext`, `SaslStep::Fail(SaslFailure)`,
  `step(Option<&[u8]>)` (нет начального ответа ≠ пустой).
- 2026-10-31 — CTR-09 (волна contracts-03): манифест индекса — `schema`, `IndexSegment::docs`, `rebuild`; `put_manifest(.., ManifestUpdate)`,
  `request_rebuild`, `pending_rebuilds`.
- 2026-10-31 — CTR-20 (решение 2026-10-31-atomic-delivery): `MessageStore::insert_batch` — все доставки приёма одной транзакцией.
- 2026-10-31 — CTR-19 (x/contracts-08): `acl::{AclStore, AclEntry, SharedMailbox, SendDelegation, ANYONE, validate_identifier, effective_rights}`
  — ACL папок и делегирование отправки (META-11) за контрактом; реализации — `PgMeta`, `tp_testkit::MemAcl`, набор `suite::acl_store`.
- 2026-11-01 — CTR-22 (x/contracts-11): `app_password::{AppPasswordStore, AppPassword}` вместо временного трейта `tp-auth` (AUTH-02);
  реализации — `PgMeta` (миграция `0009_app_passwords`), `tp_testkit::MemAppPasswords`, набор `suite::app_password_store`.
- 2026-11-01 — CTR-21 (x/contracts-10, решение 2026-11-01-borrowings-security): `blob::Compression` и методы `BlobIndex`
  `supports_compression`/`commit_new_with`/`compression` (по умолчанию — без сжатия); `load::{LoadLevel, LoadMonitor, fixed_load}`
  и поле `RoleCtx::load`; `delivery::{DeliveryLog, DeliveryEvent, DeliveryQuery, DeliveryPage}`; `groups::{GroupStore,
  DistributionGroup, HeldMessage}`, `GroupId`, `HeldId`. In-memory — `MemBlobIndex` (признак), `MemLoadMonitor`, `MemDeliveryLog`,
  `MemGroups`; наборы `suite::{blob_compression, delivery_log, group_store}`.
- 2026-11-01 — решение «Взаимодействие компонентов и требования к СУБД»: сигналы через PostgreSQL LISTEN/NOTIFY, `Rpc`, `AbuseBlocks`,
  `NodeInfo`; канал и шина Valkey удаляются (реализация — CTR-23, CLU-09).
- 2026-11-01 — решение «Состав функций транспорта и внешний MTA»: `transport::{TransportStore, MailFlow}`, `EventKind::TransportChanged`
  (реализация — CTR-24).
- 2026-11-01 — CTR-26 (x/contracts-14, решение 2026-11-01-trash-purge): `retention::{TrashPurge, TrashPurgeRule, TrashPurgeSetting,
  PurgeOutcome}`, `TrashStore::purge`/`list_hidden` (скрытые письма; `list` — без них, `restore`/`defer`/`expired` — и для скрытых),
  `Policy::trash_purge`, `PolicyResolver::trash_purge`/`set_user_trash_purge`; квота без скрытых. In-memory — `MemPolicies`,
  `MemTrash` (скрытые в `MemMetadata`); набор `suite::trash_purge`. PostgreSQL — META-29 (в `PgMeta` пока заглушки).
