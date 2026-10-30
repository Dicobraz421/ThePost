# Контракты The Post

Межкрейтовые контракты. Код — крейт `tp-contracts`; этот файл — его спецификация. Менять только PR класса B
(сначала контракт, потом волны). Сигнатуры — ориентир для CTR-карточек; точные типы фиксирует код.
Асинхронные трейты, используемые как `dyn`, — через `async-trait`; все трейты `Send + Sync + 'static`.

## Ids
- `TenantId`, `DomainId`, `AccountId`, `MailboxId`, `MessageId` — newtype над `Uuid` (v7), `Copy`, `Serialize`; `TenantId::DEFAULT` = `00000000-0000-0000-0000-000000000001` — организация по умолчанию.
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
ключи DKIM, профиль Milter) — `PolicyResolver::resolve(tenant, domain, account, mailbox?) -> Policy` по цепочке организация → домен → ящик (→ папка).

## BlobStore
SRS 3.2. Объекты неизменяемы, запись идемпотентна.
- `put(&BlobKey, Bytes) -> Result<()>`; `put_stream(&BlobKey, ByteStream, size_hint) -> Result<()>`
- `get(&BlobKey, Option<Range<u64>>) -> Result<ByteStream>`; `head(&BlobKey) -> Result<Option<u64>>`
- `delete(&BlobKey) -> Result<()>` (отсутствие — не ошибка)
- `list(prefix: &str, older_than: OffsetDateTime) -> ObjectStream` (`ObjectEntry { path, size, modified }`, `key()`) — для поиска «сирот» и миграции.
- `put_raw/get_raw/delete_raw(path)` — произвольные пути для сегментов индексов (`idx/{tenant}/{account}/...`).

## BlobIndex
SRS 3.4. Учёт объектов в метаданных. Реализация — `tp-meta`; in-memory — `tp-testkit`.
- `acquire(tenant, hash, refs: u32) -> Result<Acquire>`; `Acquire::Existing { generation }` — ссылки добавлены,
  загрузка не нужна; `Acquire::Absent { generation }` — загрузить по нему и вызвать `commit_new`.
- `commit_new(tenant, hash, generation, size, refs) -> Result<Commit>`; `Commit::Done { generation }` (письмо ссылается
  на это поколение) или `Commit::Bumped { generation }` — запись была `deleting`: загрузить по новому `gen` и повторить `commit_new`.
- `release(tenant, hash, generation, refs) -> Result<()>` — при обнулении фиксирует `zero_since`.
- `gc_candidates(older_than, limit) -> Result<Vec<BlobKey>>`; `mark_deleting(&BlobKey, older_than) -> Result<bool>` (перепроверяет окно W);
  `finalize_delete(&BlobKey) -> Result<bool>` — удаляет запись, только если `gen` не изменился.
- `exists(&BlobKey) -> Result<bool>` — для поиска «сирот».

## Metadata
SRS 3.1, 3.6. Все операции ящика принимают `AccountScope { tenant, account }`; без связей между ящиками.
`ModSeq` выдаётся из одного счётчика учётной записи: монотонен в папке (CONDSTORE) и в учётной записи (состояние JMAP).
- `AccountStore`: `get`, `by_login`, `upsert` (из LDAP), `quota`.
- `MailboxStore`: `list(account)`, `get`, `create`, `rename`, `delete`, `set_subscribed`, `status` (counts, uidnext, highest modseq).
- `MessageStore`: `insert(scope, mailbox, NewMessage) -> Inserted { id, uid, modseq }`; `get`, `list(mailbox, MessageQuery, limit)`,
  `set_flags(ids, FlagOp, unchanged_since) -> FlagsUpdate`, `copy`, `move_to`, `to_trash`, `restore`, `expunge_final`.
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
- `ingest(IngestRequest { tenant, raw: ByteStream, deliveries: Vec<Delivery { account, mailbox, extra_headers, flags }>, received_at })
  -> Result<Vec<Delivered { account, mailbox, message, uid }>>` — одна загрузка частей на всех получателей;
  квота одного получателя — `QuotaExceeded` на весь запрос.
- `fetch_raw(AccountScope, message, Option<Range>) -> ByteStream`; `fetch_part(AccountScope, message, part_path)` — тело в кодировке передачи.
- `set_flags`, `copy`, `move_to`, `delete_to_trash`, `restore` (по `AccountScope`) — публикуют событие в `EventBus` после фиксации.

## EventBus
SRS 2.1. `publish(Event)`, `subscribe(AccountScope) -> EventStream`, `subscribe_system()`. `Event { tenant, account: Option, kind }`,
`kind: MailboxChanged { mailbox, modseq } | MailboxListChanged | QueueReady | ConfigReloaded`.
Реализации: в процессе (`tp-cluster`, SMB), Valkey (`tp-cluster`, Enterprise). Доставка — at-most-once;
клиент после переподключения сверяется по `ModSeq`.

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

## Search
SRS 4.6. `SearchIndex`: `query(AccountScope, mailboxes: &[MailboxId] (пусто — все), &TextQuery, SearchSort, limit) -> Vec<MessageId>`,
`snippet(AccountScope, message, &TextQuery) -> Snippet { subject?, preview? }` (`<mark>`). `TextQuery` — дерево
`Term { field: TextField, text } | And | Or | Not`; нетекстовые условия фильтрует вызывающий по метаданным.
`IndexManifestStore` (метаданные): `get_manifest(AccountScope) -> IndexManifest { version, segments }`,
`put_manifest(AccountScope, expected, segments) -> version` (несовпадение версии — `Conflict`).

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
SASL-механизмы (PLAIN, LOGIN, OAUTHBEARER, XOAUTH2, GSSAPI) — `tp-auth`, интерфейс `SaslServer { mechanisms(tls), start(mech) }`
→ `SaslSession::step(response) -> SaslStep::{ Challenge, Done(Principal) }`.

## Roles
SRS 2.1. `enum Role { SmtpIn, Submission, SmtpOut, Imap, Jmap, Dav, Admin, Search, Indexer, Maintenance }`; пресеты (`all`, `edge`, `transport`, `access`, `backend`, `mailbox`) раскрываются в набор ролей при загрузке конфигурации; режим edge — параметр smtp-in (SRS 2.6).
Раскрытие — `RoleSet::expand(&[RoleEntry]) -> RoleSet { roles, edge }`; `RoleEntry` = пресет | роль (строка config.yaml).
`RoleService`: `name()`, `run(ctx: RoleCtx) -> Result<()>` до сигнала отмены; `RoleCtx { config: Arc<dyn ConfigWatch>,
shutdown: Shutdown, drain_deadline, ready: ReadyHandle }`. `ConfigWatch` — `current() -> Arc<dyn Any>`, `changed()`; типизированно —
`RoleCtx::config::<T>()`. `Shutdown` — на `futures` (tokio в `tp-contracts` не подключён); метрики — позже.

## Config
Каждый крейт владеет своей секцией `XxxConfig: Deserialize + Default + Clone` и документирует её поля.
Приложение собирает общий `Config`; обязательны только `database.url`, `blob.url`, `domain` (SRS 5.5).
Hot reload: новая копия рассылается через `watch`; параметры, требующие перезапуска, помечены в doc-комментарии.

## Queue (Ф1)
`QueueStore`: `enqueue(OutMessage { blob, from, rcpts, next_attempt })`, `claim(limit, lease)`,
`reschedule(id, rcpt, at, reason)`, `complete(id, rcpt, status)`. Повторы и DSN — `tp-smtp`.

## Trash, Quota (Ф1)
`TrashStore`: `list(account)`, `restore(item)`, `expired(before, limit)`; срок — `PolicyResolver`. `Quota`: `usage/limit(account)`.
Политики хранения и Legal Hold — модуль «Соответствие требованиям» через `DecisionHook::before_final_delete`.

## Extension points
SRS 2.5, 1.3. Модуль — cargo feature + `modules.<имя>.enabled`. `Module { name(), schema() -> "mod_<имя>", migrations(), register(&mut Registry, &ModuleCtx) }`.
`Registry`: `add_role`, `add_jmap_capability`, `add_admin_routes`, `add_event_sink`, `add_decision_hook`, реализации трейтов (`BlobStore`, `Directory`, `SearchIndex`).
`DecisionHook`: `before_final_delete`, `on_delivery`, `on_submission` → `Decision::{ Allow, Deny(reason), Augment(headers|copies) }`;
без модулей — `Allow`. Данные модуля — только в схеме `mod_<имя>` с собственными миграциями.

## CHANGELOG
- 2026-10-29 — первая редакция (по SRS ред. 3).
- 2026-10-30 — ред. 4: Tenant, Events (журнал событий вместо Outbox), SearchIndex, Directory, Extension points; Legal Hold — модуль.
- 2026-10-30 — CTR-01…04 (волна contracts-01): `generation` вместо `gen`, `Commit::Done { generation }`, `mark_deleting(.., older_than)`,
  `ObjectEntry`, `AccountScope`, `ModSeq` учётной записи, `Changes<T>`, системная подписка шины, fencing лиз, `EventLog` без `append`,
  `RoleSet`/`RoleEntry`, `ConfigWatch`/`Shutdown` вместо tokio-типов.
