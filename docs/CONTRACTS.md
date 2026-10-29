# Контракты The Post

Межкрейтовые контракты. Код — крейт `tp-contracts`; этот файл — его спецификация. Менять только PR класса B
(сначала контракт, потом волны). Сигнатуры — ориентир для CTR-карточек; точные типы фиксирует код.
Асинхронные трейты, используемые как `dyn`, — через `async-trait`; все трейты `Send + Sync + 'static`.

## Ids
- `TenantId`, `AccountId`, `MailboxId`, `MessageId` — newtype над `Uuid` (v7), `Copy`, `Serialize`.
- `Uid(u32)` — IMAP UID в ящике, монотонный; `UidValidity(u32)`.
- `ModSeq(u64)` — монотонный на ящик (IMAP CONDSTORE, состояние JMAP).
- `BlobHash([u8; 32])` — SHA-256, `Display` — hex. `Gen(u32)` — поколение объекта.
- `BlobKey { tenant, hash, gen }` → путь объекта `"{tenant}/{hash[0..2]}/{hash}/{gen}"`.

## Errors
`enum Error { NotFound, Conflict, Invalid(String), Forbidden, QuotaExceeded, Unavailable { retry_after: Option<Duration> }, Internal(String) }`;
`type Result<T> = std::result::Result<T, Error>`. `Unavailable` — повторяемая (сеть, БД, S3).

## BlobStore
SRS 3.2. Объекты неизменяемы, запись идемпотентна.
- `put(&BlobKey, Bytes) -> Result<()>`; `put_stream(&BlobKey, ByteStream, size_hint) -> Result<()>`
- `get(&BlobKey, Option<Range<u64>>) -> Result<ByteStream>`; `head(&BlobKey) -> Result<Option<u64>>`
- `delete(&BlobKey) -> Result<()>` (отсутствие — не ошибка)
- `list(prefix: &str, older_than: OffsetDateTime) -> ByteKeyStream` — для поиска «сирот» и миграции.
- `put_raw/get_raw(path)` — произвольные пути для сегментов индексов (`idx/{tenant}/{account}/...`).

## BlobIndex
SRS 3.4. Учёт объектов в метаданных. Реализация — `tp-meta`; in-memory — `tp-testkit`.
- `acquire(tenant, hash, refs: u32) -> Result<Acquire>`; `Acquire::Existing { gen }` — ссылки добавлены,
  загрузка не нужна; `Acquire::Absent { gen }` — загрузить по `gen` и вызвать `commit_new`.
- `commit_new(tenant, hash, gen, size, refs) -> Result<Commit>`; `Commit::Done` или
  `Commit::Bumped { gen }` — запись была `deleting`: загрузить по новому `gen` и повторить `commit_new`.
- `release(tenant, hash, gen, refs) -> Result<()>` — при обнулении фиксирует `zero_since`.
- `gc_candidates(older_than, limit) -> Result<Vec<BlobKey>>`; `mark_deleting(&BlobKey) -> Result<bool>`;
  `finalize_delete(&BlobKey) -> Result<bool>` — удаляет запись, только если `gen` не изменился.
- `exists(&BlobKey) -> Result<bool>` — для поиска «сирот».

## Metadata
SRS 3.1, 3.6. Все операции ящика принимают `AccountId`; без связей между ящиками.
- `AccountStore`: `get`, `by_login`, `upsert` (из LDAP), `quota`.
- `MailboxStore`: `list(account)`, `get`, `create`, `rename`, `delete`, `set_subscribed`, `status` (counts, uidnext, highest modseq).
- `MessageStore`: `insert(account, mailbox, NewMessage) -> (MessageId, Uid, ModSeq)`; `get`, `list(range|uids)`,
  `set_flags(..., FlagOp) -> ModSeq`, `copy`, `move_to`, `to_trash`, `restore`, `expunge_final`.
- `NewMessage`: `skeleton: BlobKey`, `parts: Vec<PartRef>`, `size`, `received_at`, `flags`, `meta: MessageMeta`
  (envelope, bodystructure, preview, thread key — вычисляются при приёме).

## ChangeLog
Журнал изменений для IMAP QRESYNC и JMAP `/changes`: `changes(account, since: ModSeq|State, limit)
-> Changes { created, updated, destroyed, new_state }`. Пишется в той же транзакции, что изменение.

## MailStore
Высокоуровневый API, крейт `tp-mailstore` (полоса mailstore). Протоколы (smtp, imap, jmap, admin) работают через него.
- Модель «скелет + части»: письмо = объект-скелет (заголовки, границы MIME, ссылки) + объекты частей.
  Побайтовая реконструкция исходного письма обязательна.
- `ingest(IngestRequest { tenant, raw: ByteStream, deliveries: Vec<Delivery { account, mailbox, extra_headers, flags }>, received_at })
  -> Result<Vec<Delivered { account, mailbox, message, uid }>>` — одна загрузка частей на всех получателей.
- `fetch_raw(account, message, Option<Range>) -> ByteStream`; `fetch_part(account, message, part_path)`.
- `set_flags`, `copy`, `move_to`, `delete_to_trash`, `restore` — публикуют событие в `EventBus`.

## EventBus
SRS 2.1. `publish(Event)`, `subscribe(account) -> EventStream`. `Event { tenant, account, kind }`,
`kind: MailboxChanged { mailbox, modseq } | MailboxListChanged | QueueReady | ConfigReloaded`.
Реализации: в процессе (`tp-cluster`, SMB), Valkey (`tp-cluster`, Enterprise). Доставка — at-most-once;
клиент после переподключения сверяется по `ModSeq`.

## Lease
`LeaseStore`: `try_acquire(name, holder, ttl) -> Option<Lease>`, `renew`, `release`. Лидер maintenance,
индексатор ящика, записи очереди. Реализация — PostgreSQL.

## Outbox
Задания для фоновых ролей: `push(kind, key, payload)` в транзакции изменения; `claim(kind, limit, lease_ttl)`
(`SKIP LOCKED`), `done`, `retry_at`. Виды: `IndexMessage`, `DeindexMessage`, `ReindexMailbox`.

## Search
SRS 4.6. `SearchBackend`: `query(account, scope: Vec<MailboxId>, q: TextQuery, sort, limit) -> Vec<MessageId>`,
`snippet(account, message, q)`. `IndexManifestStore` (метаданные): `get/put_manifest(account) -> (version, segments)`.

## Auth
SRS 5.1. `Authenticator`: `verify(Credentials) -> Result<Principal>`;
`Credentials::{ Password { login, secret }, Bearer(token), Gssapi(ctx) }`. `Principal { tenant, account, groups, scopes }`.
`Authorizer`: `check(&Principal, Resource, Rights) -> Result<()>`; права — RFC 4314 (`lrswipkxtea`), Send As / Send on Behalf.
SASL-механизмы (PLAIN, LOGIN, OAUTHBEARER, XOAUTH2, GSSAPI) — `tp-auth`, интерфейс `SaslServer` (шаги challenge/response).

## Roles
SRS 2.1. `enum Role { SmtpIn, SmtpOut, Imap, Jmap, Dav, Admin, Search, Indexer, Maintenance }`.
`RoleService`: `name()`, `run(ctx: RoleCtx) -> Result<()>` до сигнала отмены; `RoleCtx { config: watch::Receiver<Arc<…>>,
shutdown: CancellationToken, drain_deadline, ready: ReadyHandle, metrics }`.

## Config
Каждый крейт владеет своей секцией `XxxConfig: Deserialize + Default + Clone` и документирует её поля.
Приложение собирает общий `Config`; обязательны только `database.url`, `blob.url`, `domain` (SRS 5.5).
Hot reload: новая копия рассылается через `watch`; параметры, требующие перезапуска, помечены в doc-комментарии.

## Queue (Ф1)
`QueueStore`: `enqueue(OutMessage { blob, from, rcpts, next_attempt })`, `claim(limit, lease)`,
`reschedule(id, rcpt, at, reason)`, `complete(id, rcpt, status)`. Повторы и DSN — `tp-smtp`.

## Trash, Retention, Quota (Ф1)
`TrashStore`: `list(account)`, `restore(item)`, `expired(before, limit)`. `RetentionPolicy` — срок по уровню
(организация, группа, ящик, папка); `LegalHold` блокирует окончательное удаление. `Quota`: `usage/limit(account)`.

## CHANGELOG
- 2026-10-29 — первая редакция (по SRS ред. 3).
