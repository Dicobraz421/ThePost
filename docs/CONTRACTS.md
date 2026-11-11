# Контракты The Post

Межкрейтовые контракты. Код — крейт `tp-contracts`; этот файл — его спецификация. Менять только PR класса B
(сначала контракт, потом волны). Сигнатуры — ориентир для CTR-карточек; точные типы фиксирует код.
Асинхронные трейты, используемые как `dyn`, — через `async-trait`; все трейты `Send + Sync + 'static`.

## Ids
- `TenantId`, `DomainId`, `AccountId`, `MailboxId`, `MessageId`, `QueueId`, `GroupId`, `HeldId` — newtype над `Uuid` (v7), `Copy`, `Serialize`; `TenantId::DEFAULT` = `00000000-0000-0000-0000-000000000001` — организация по умолчанию.
- `Uid(u32)` — IMAP UID в ящике, монотонный; `UidValidity(u32)`.
- `ShardId(i32)` — шард метаданных (кластер PostgreSQL, SRS 3.6); `ShardId::MAIN` = 0 — основная БД с каталогом (CTR-34); `tp_meta::shards::ShardId` переходит на него (волна meta).
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
- Срок GC (CTR-38): `next_gc_due(window) -> Option<OffsetDateTime>` — `MIN(zero_since) + window` среди кандидатов
  `gc_candidates` (`deleting` — тоже; все организации), `None` — их нет; таймер GC взводится на него. По умолчанию — деление
  пополам по `gc_candidates(.., 1)` с точностью до секунды (срок не раньше точного); реализации — запрос `MIN` по индексу.
- Владелец загрузки (CTR-38, JMAP `uploadUrl`): загруженный файл без ссылок доступен только загрузившей учётной записи; учёт —
  общий для узлов и переживает перезапуск. `mark_upload(AccountScope, &BlobKey, until)` (повтор — до большего срока;
  `key.tenant` ≠ `scope.tenant` — `Invalid`), `upload_owned(scope, &key, now) -> bool` (`until > now`; существование —
  `exists`), `prune_uploads(now, limit) -> usize` — удалить отметки с `until <= now` (все организации), вызывает GC в проходе.
  По умолчанию: `mark_upload` — `Internal`, `upload_owned` — `false`, `prune_uploads` — `0`. In-memory — `MemBlobIndex`;
  наборы — `suite::{blob_gc_due, blob_uploads}`.

## Metadata
SRS 3.1, 3.6. Все операции ящика принимают `AccountScope { tenant, account }`; без связей между ящиками.
Шарды (решение 2026-11-01-scale-shards-geo-admin): все хранилища ящика получают пул через `ShardResolver::pool_for(scope, intent)` (`tp-meta`);
один шард — вырожденный случай без запроса к справочнику. `ReadIntent::{ Primary, Stale }`: `Stale` — только закрытый список фоновых чтений
(экспорт, отчёты, eDiscovery, verify, «сироты», первичная индексация), реплика шарда при отставании не больше порога, иначе primary;
реализации для фоновых ролей собираются с `Stale` (`PgMeta::with_intent`), контракты трейтов не меняются.
CTR-25: раздел журнала событий — `Partition { shard, index }`; разделы перечисляются по всем шардам справочника.
`ModSeq` выдаётся из одного счётчика учётной записи: монотонен в папке (CONDSTORE) и в учётной записи (состояние JMAP).
- `AccountStore`: `get`, `by_login`, `by_external_id` (идентификатор каталога, точное совпадение; CTR-32 — синхронизация каталога
  сопоставляет по нему, переименование входа не меняет `AccountId` и членство в группах; по умолчанию — `Internal`),
  `list(tenant, prefix, after, limit)` (CTR-44, список Admin API: по возрастанию `lower(login)` в порядке сравнения строк хранилища,
  префикс и курсор `after` — в нижнем регистре, строго больше; по умолчанию — `Internal`), `upsert` (из LDAP), `quota`.
- `MailboxStore`: `list(account)`, `get`, `create`, `rename`, `delete(scope, id, cascade)`, `set_subscribed`, `status` (counts, uidnext, highest modseq).
  `delete` папки с вложенными без `cascade` — `Conflict`: решение («удалить вместе с вложенными?») принимает пользователь
  в клиенте, клиент повторяет вызов с `cascade = true`; письма всего поддерева — в корзину.
- `MessageStore`: `insert(scope, mailbox, NewMessage) -> Inserted { id, uid, modseq }` (APPEND, импорт);
  `insert_batch(tenant, Vec<(AccountScope, MailboxId, NewMessage)>) -> Vec<Inserted>` — все доставки одного приёма
  одной транзакцией: вставлены все или ни одного, события доставки — в ней же (решение 2026-10-31-atomic-delivery); `get`, `list(mailbox, MessageQuery, limit)`,
  `set_flags(ids, FlagOp, unchanged_since) -> FlagsUpdate`, `copy`, `move_to`, `to_trash`, `restore`, `expunge_final`.
  `copy` создаёт новые `MessageId`; `move_to`, `to_trash`, `restore` сохраняют `MessageId` (меняются папка и UID; `move_to` — CTR-29,
  id письма JMAP неизменен по RFC 8621; событие `Moved { from_message = message }`);
  `expunge_final` удаляет только письма в корзине (остальные пропускает). `to_trash` (и `delete_to_trash`, удаление папки в корзину) снимает
  флаг `\Deleted`, остальные флаги сохраняет (CTR-33): EXPUNGE в корзине не трогает письмо, удалённое из другой папки.
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
- `append(AccountScope, mailbox, raw: ByteStream, flags, received_at) -> Delivered` — письмо клиента (IMAP APPEND, JMAP `Email/import`;
  решение 2026-11-01-trash-purge): разбор и загрузка, как у `ingest`, квота (`QuotaExceeded` до загрузки), `MessageStore::insert`,
  событие в `EventBus`. Не доставка: без `DecisionHook::on_delivery`, без копий и добавленных заголовков; нет папки — `NotFound`.
- `fetch_raw(AccountScope, message, Option<Range>) -> ByteStream`; `fetch_part(AccountScope, message, part_path)` — тело в кодировке передачи.
- `set_flags`, `copy`, `move_to`, `delete_to_trash`, `restore` (по `AccountScope`) — публикуют событие в `EventBus` после фиксации.
  `restore` восстанавливает и скрытые письма (`trash.purge: hide`; CTR-38) — в исходную папку или «Входящие», прежний `MessageId`;
  Admin API восстанавливает через него, а не через `TrashStore::restore`, чтобы клиенты получили `MailboxChanged`.
  Письмо не в корзине и не скрыто — `Invalid`.

## EventBus
SRS 2.1, 2.8 (решение 2026-11-01). Сигналы «изменилось X»: `publish(Event)`, `subscribe(AccountScope) -> EventStream`, `subscribe_system()`.
`Event { tenant, account: Option, kind }`, `kind: MailboxChanged { mailbox, modseq } | MailboxListChanged | QueueReady | ConfigReloaded |
LogAppended { partition } | MembersChanged | Blocked { subject } | Unblocked { subject } | TransportChanged { tenant } | TrashChanged { account } | Resync`. Виды учётной записи (`account = Some`) — `MailboxChanged`,
`MailboxListChanged`; остальные — системные (`EventKind::is_system`, `account = None`, приходят в `subscribe_system()`; `LogAppended`,
`MembersChanged` — с `TenantId::DEFAULT`, `Blocked`, `Unblocked` — с организацией блокировки, `TransportChanged` — с организацией или `TenantId::DEFAULT`, `TrashChanged` — с организацией учётной записи). `Resync` вставляет шина в поток подписки (её `tenant`/`account`)
при переполнении буфера подписчика или переподключении слушателя — подписчик сверяется с хранилищем; `publish(Resync)` — `Invalid`.
Реализации: в процессе (`tp-cluster`), PostgreSQL LISTEN/NOTIFY (`tp-meta`, кластер); in-memory — `MemEventBus` (отставание — `Resync`).
Доставка — at-most-once; клиент при запуске, после переподключения и по `Resync` сверяется с хранилищем. Публикация — только после фиксации,
вне транзакции изменения. `MembersChanged` публикует реализация `Membership`, `Blocked`, `Unblocked` — реализация `AbuseBlocks`, `TransportChanged` — `TransportStore`, `TrashChanged` — `MailStore::delete_to_trash` и `TrashStore::purge`. Набор `suite::signals`.
Шина Valkey удаляется (CLU-09).

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
Шарды (CTR-34, решение 2026-11-01-scale-shards-geo-admin): журнал — на шарде ящика, у каждого шарда свои разделы `0..N` и позиции
подписчиков. Раздел инсталляции — `ShardPartition { shard: ShardId, index: Partition }` (`ShardPartition::of(shard, account, N)`:
`index` = `partition_of`, шард — по справочнику шардов; порядок — шард, затем раздел; `Display` — `"{shard}/{index}"`). Журналы шардов —
`ShardEventLogs`: `shards()` (по возрастанию, всегда с `ShardId::MAIN`), `log(shard) -> Option<Arc<dyn EventLog>>`, `partitions()`
(одинаково для всех шардов); один журнал — `SingleShard`. Назначение пар — `PartitionAssigner::assigned_sharded(subscriber, &NodeId,
shards, N) -> Vec<ShardPartition>` (метод по умолчанию): раздел шарда назначается по ключу `shard_subscriber(subscriber, shard)` — для
шарда 0 само имя, иначе `"{subscriber}@{shard}"`; лиза раздела — `events/{ключ}/{index}`. Каждый раздел каждого шарда — ровно одному
узлу состава; один шард — прежние назначение и имена лиз. In-memory — `MemShardedEventLog` (`place` — справочник), `MemPartitionAssigner`;
наборы `suite::partition_assigner`, `suite::shard_event_logs`. Сигнал `LogAppended { partition }` пока без шарда.

## Membership
Решение 2026-10-30, #45. `Membership`: `heartbeat(&NodeId, ttl)` — создать или продлить запись узла (период ≈ `ttl / 3`),
`leave(&NodeId)` — удалить (штатная остановка; отсутствующая — не ошибка), `live() -> Vec<NodeId>` — узлы с непросроченной записью,
по возрастанию. Время — часы хранилища (PostgreSQL). `NodeId` — имя экземпляра, не пустое, без пробелов, ≤ 255 байт (строка в JSON).
Реализация — `tp-meta` (META-16), in-memory — `tp-testkit`. Код — `membership.rs`.
CTR-23 (решение 2026-11-01): `heartbeat(&NodeInfo, ttl)` — запись узла `NodeInfo { id, roles: BTreeSet<Role>, addr, site: SiteId }`
(роли, адрес служебного канала `https://host:port`, пусто — запросов не принимает; `SiteId` — из конфигурации узла, по умолчанию `default`,
правила имени — как у `NodeId`); продление заменяет роли, адрес и площадку. `live_info() -> Vec<NodeInfo>` — живые записи по возрастанию `id`;
`live()` — их идентификаторы (метод по умолчанию). Новая запись (и после истечения), изменение записи и выход живого узла — сигнал
`MembersChanged` после фиксации; истечение — без сигнала (срок аренды). `PartitionAssigner::members()` обновляется по сигналу, а не опросом
(CLU-10). In-memory — `MemMembership` (`with_bus` — сигналы); наборы `suite::membership`, `suite::membership_info`. В `PgMeta` роли, адрес,
площадка и `live_info()` — META-27.

## Rpc
Решение 2026-11-01, SRS 2.8 (CTR-23). Запрос узел → узел: `Rpc::call(&RpcService, key, body: Bytes, deadline: Duration) -> Result<Bytes, RpcError>`;
`RpcService::new(role, name)` — роль и имя службы (`[a-z0-9._-]`, ≤ 64; `search/query`), маршрут — `live_info()` с ролью службы, владелец ключа —
rendezvous-хэширование, при отказе узла — следующий. `RpcError`: `Busy` (параллельность на узле-владельце превышена; без повтора на другом
узле), `Timeout` (срок), `Unavailable` (нет живых узлов роли или все недоступны), `Failed(Error)` (ошибка обработчика; `Forbidden` — матрица
разрешений, `NotFound` — служба не зарегистрирована на узле); `Error::from(RpcError)`: первые три — `Unavailable` (временный отказ),
`Failed(e)` — `e`. Обработчик — `RpcHandler::handle(RpcRequest { caller, key, body, deadline }) -> Result<Bytes>`, регистрируется ролью
через `Rpc::serve(service, handler)` (повтор — `Conflict`); матрица разрешений «роль → службы» проверяется на принимающем узле по сертификату
вызывающего. Транспорт — HTTPS/2 с mTLS (`InternalChannel::Node`). Реализация — `tp-cluster` (CLU-11), in-memory — `MemRpcNet`/`MemRpc`;
набор `suite::rpc`.

## AbuseBlocks
Решение 2026-11-01, SRS 6.3 (CTR-23). Блокировки источников и учётных записей организации: `block(tenant, &subject, until, reason)`
(срок в прошлом — `Invalid`; действующая блокировка не сокращается, причина — последняя), `blocked(tenant, &subject) -> Option<until>`
(наибольший действующий срок с учётом подсетей, покрывающих адрес), `unblock(tenant, &subject)` (только этот субъект; отсутствующий — не
ошибка), `active(tenant) -> Vec<AbuseBlock { subject, until, reason }>` (локальный набор узла при запуске и `Resync`). `BlockSubject:
Ip(IpAddr) | Net { addr, prefix } | Account(AccountId)`; подсеть — `BlockSubject::net(addr, prefix)` (биты узла обнулены), `covers()`.
Время — часы хранилища. Счётчики попыток — на узле (`tp-auth`, AUTH-08); факт блокировки — в PostgreSQL (`tp-meta`, META-27), после
фиксации `block` реализация публикует `Blocked { subject }`, после фиксации `unblock` — `Unblocked { subject }` (CTR-39: узлы убирают
субъект из локального набора, не дожидаясь срока; для отсутствующей блокировки сигнал не обязателен; истечение — без сигнала).
In-memory — `MemAbuseBlocks` (`with_bus`), наборы `suite::abuse_blocks`, `suite::abuse_blocks_signals`.

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
Решения модератора (CTR-31): `Moderation` (вместо временного трейта `tp-admin`) — `deliver(tenant, &group, &HeldMessage)` доставляет
одобренное письмо участникам группы (раскрытие `expand`, приём в ящики, журнал доставки; другая группа или организация — `Invalid`,
проверка — `check_delivery`), `reject(tenant, &HeldMessage, reason)` — DSN отправителю (`validate_reject_reason`: одна строка
1–500 символов, иначе `Invalid`; пустой отправитель — без DSN). Письмо уже забрано `take_held`; успех освобождает ссылку на объект,
ошибка — вызывающий возвращает письмо `hold`. Реализация — `tp-smtp` (карточка smtp), in-memory — `MemModeration` (запись решений,
`fail_next`); набор `suite::moderation`.

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
Адрес для всей почты домена (catch-all) — псевдоним `DirAlias { address: "@domain", .. }`: получатель на адрес домена, не найденный
в каталоге; порядок разрешения — адрес целиком, адрес без подадреса (`user+tag`), `@domain`.
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
Локальный токен Admin API (код — `admin_token.rs`, CTR-41, решение #193): первая учётная запись SMB без OIDC — `thepost admin init` на узле
выпускает одноразовый токен с коротким сроком (`admin.init.token_ttl`); Admin API принимает его в `Authorization: Bearer` наравне с OIDC —
значение с префиксом `LOCAL_ADMIN_TOKEN_PREFIX` (`"tpa_"`, `is_local_admin_token`). Значение выдаётся один раз, хранится хэш (hex SHA-256 в
нижнем регистре); значение, хэш и проверку ведёт `tp-admin`. `LocalAdminToken { id, tenant, account, hash, created_at, expires_at, used_at?,
revoked_at? }` (`Debug` без хэша; `validate()`: id 1–64 байт без пробелов и управляющих, хэш — 64 hex, срок позже выпуска → `Invalid`;
`state(now) -> LocalTokenState::{Active, Used, Revoked, Expired}`). `LocalAdminTokens` (основная БД): `issue(token)` — занятые id или хэш —
`Conflict`, `used_at`/`revoked_at` заданы — `Invalid`; `consume(hash, now) -> LocalAdminToken` — атомарное погашение (из параллельных успешен
один), нет/погашен/отозван/истёк (`now ≥ expires_at`) — `Forbidden`; `revoke(id, at)` — нет — `NotFound`, повтор сохраняет первый момент;
`list(tenant)` — все состояния по (`created_at`, `id`); `prune(before)` — удаляет с `expires_at < before`, возвращает число.
In-memory — `MemLocalAdminTokens`; набор `suite::local_admin_tokens`. PostgreSQL — `tp-meta` (карточка meta).

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
SRS 4.1, 2.6; решения 2026-11-01-transport-scope-external-mta, 2026-11-01-scale-shards-geo-admin. Код — `transport.rs` (CTR-24).
Настройки транспорта — данные основной БД PostgreSQL, Admin API; узлы держат снимки в памяти и перечитывают их по сигналу
`TransportChanged { tenant: Option<TenantId> }` (`None` — уровень кластера, `Event::tenant` — `TenantId::DEFAULT`; сигнал —
`transport::transport_changed(tenant)`) и при `Resync`. В config.yaml — только параметры узла.
`TransportStore`: `cluster() -> ClusterTransport { version, domains: Vec<AcceptedDomain>, sources: Vec<TrustedSource> }` — для выбора
организации и источника до аутентификации; `tenant(tenant) -> TenantTransport { version, routes: Vec<DestinationPolicy>, rewrites: Vec<RewriteRule> }` (без записей —
версия 0, пусто; чужие записи не видны). Списки — по возрастанию ключа.
- `AcceptedDomain { id: DomainId, tenant, name, kind: DomainKind::{ Authoritative (по умолчанию), InternalRelay { next_hop } } }` — имя
  уникально в кластере, каноническое (`normalize_domain`: ASCII в нижнем регистре, без точки в конце, IDN — A-label);
  `ClusterTransport::domain(name)` — поиск без учёта регистра, поддомены — только явно.
- `TrustedSource { name ([a-z0-9._-], ≤ 64), tenant: Option<TenantId> (None — источник кластера), matcher: SourceMatch::{ Networks(Vec<IpNet>),
  ClientCert { subject } (dNSName проверенного сертификата mTLS) }, kind: SourceKind::{ Gateway { xclient, authserv_id: Option<String> },
  Application { senders: Vec<AddressPattern>, external } } }`; источник приложения — только с организацией и непустыми `senders`.
  `ClusterTransport::source(ip, cert_names)` — наиболее точный (`SourceMatch::score`: сертификат точнее подсети, длинный префикс точнее),
  при равенстве — первый по имени. `IpNet` — подсеть со сброшенными битами узла, в JSON строка `"192.0.2.0/24"`; `AddressPattern` —
  `user@domain` или `@domain` (все адреса домена), в JSON строка.
- `DestinationPolicy { pattern, tls: TlsRequirement::{ Opportunistic (по умолчанию), Required, Pinned { spki_sha256 } }, smart_host?, egress_ip? }`;
  `pattern` — ключ в организации: `example.org`, `*.example.org` (только поддомены), `*` (все внешние, relayhost).
  `TenantTransport::route(domain)` — наиболее точный шаблон (домен → длинный суффикс → `*`), только для доменов вне принятых;
  `policy(pattern)` — по шаблону.
- `RewriteRule { from: AddressPattern (ключ), to: AddressPattern, sender, recipient (по умолчанию true) }` — таблица переписывания адресов
  конверта организации (модель Postfix `canonical_maps`; не config.yaml): `to` = `@domain` сохраняет локальную часть; `validate()` —
  хотя бы одна сторона, `to ≠ from`. `TenantTransport::rewrite(address, EnvelopeSide::{ Sender, Recipient })` — правило для адреса
  точнее правила для домена, одно правило без цепочек; отправитель — после SPF, DKIM, DMARC, получатели — до разрешения в каталоге;
  заголовки не меняются (это — `MailFlow`).
Запись — `put_domain`/`delete_domain`/`put_source`/`delete_source(expected, ..)` (версия кластера) и `put_route`/`delete_route`/
`put_rewrite`/`delete_rewrite(tenant, expected, ..)` (версия организации) `-> новая версия` (строго больше; `*_rewrite` по умолчанию —
`Internal`). Устаревшая `expected` — `Conflict` (до проверки ключа), отсутствующий ключ при
удалении — `NotFound`, поля — `validate()` типа (`Invalid`); имя домена занято другим `id` — `Conflict`; смена организации домена или
источника — `Invalid`. После фиксации — `transport_changed` (домены и источники — `None`, политики — организация).
Реализация — `tp-meta`, in-memory — `MemTransport` (`with_bus`); наборы `suite::{transport_store, transport_rewrites}`.

`MailFlow` — точка расширения правил потока почты (реализация — модуль «Правила потока почты», `Registry::set_mail_flow`; правила, их
хранение в схеме `mod_mailflow` и Admin API — модуль; без модуля не вызывается): `apply(&FlowInput { tenant, direction: FlowDirection::{
Inbound, Submission }, principal: Option<Principal>, source: Option<String> (доверенный источник), envelope: FlowEnvelope { mail_from,
rcpt_to }, message: BlobRef { key: BlobKey, size } }) -> FlowOutcome { verdict: FlowVerdict::{ Accept, Reject(text), Drop, Moderate {
moderators } }, envelope, message: Option<BlobRef>, route: Option<String>, matched: Vec<String> }` (`FlowOutcome::unchanged(&input)`).
Один раз на письмо (smtp-in после Milter, submission, JMAP EmailSubmission), до `DecisionHooks`, доставки или очереди; письмо с получателями
нескольких организаций — вызов на организацию. Изменённое тело — новый объект BLOB, записанный модулем (ссылку учитывает вызывающий);
`Reject` — SMTP 550, JMAP `forbiddenToSend` (RFC 8621); `Accept` с пустым `rcpt_to` — как `Drop`; `route` — шаблон политики домена назначения организации (нет такой — обычный выбор);
`matched` — сработавшие правила, включая режим проверки (журнал доставки). Ошибка — ошибка операции (`Unavailable` — временный отказ).

## Queue (Ф1)
SRS 4.1. Код — `queue.rs`. `QueueStore` (общая для организаций; время передаёт вызывающий):
`enqueue(OutMessage { tenant, blob, size, from, dsn: DsnParams { envid, ret }, rcpts: Vec<OutRcpt { address, notify, orcpt }>, next_attempt }) -> QueueId`
(пусто или повтор адреса — `Invalid`); `claim(now, limit, lease) -> Vec<Claimed { claim: QueueClaim { id, fencing }, until, queued_at, .., rcpts: Vec<QueuedRcpt { rcpt, attempts, last_error }> }>`
— записи с наступившим сроком попытки без действующей лизы; `renew(&claim, until)`, `reschedule(&claim, rcpt, at, reason)` (`attempts` + 1),
`postpone(&claim, rcpt, at)` (перенос планировщиком без попытки — лимиты домена или узла: `attempts`, `last_error` не меняются; по
умолчанию — `Internal`; набор `suite::queue_postpone`),
`complete(&claim, rcpt, RcptStatus::{ Delivered { reply }, Failed { reply } }) -> bool` (`true` — все получатели завершены, запись удалена).
Устаревший захват — `Conflict`; не завершённые до истечения лизы получатели возвращаются в очередь без `attempts` + 1.
Тело — один объект BLOB: ссылку учитывает вызывающий до `enqueue` и освобождает после `complete` = `true`.
Расписание повторов, срок доставки и DSN — `tp-smtp` (DSN ставится в очередь до `complete(.., Failed)`).
Постановка письма протоколами — `Outbound` (CTR-28): `enqueue(tenant, mail_from, rcpts, raw: Bytes)`;
`submit(..) -> Submitted { rcpts: Vec<(String, SubmitStatus::{ Delivered, Queued, Failed(reply) })> }` — статус по получателю в порядке
запроса (по умолчанию — `enqueue`, все `Queued`). Реализации — `tp_smtp::queue::Queue` (тело в BLOB, ссылка, подпись DKIM `with_dkim`),
`tp_smtp::Delivery` (локальные — сразу в ящики); потребители — submission SMTP, пересылки модулей, JMAP `EmailSubmission/set`.
Маршрут правила потока (`FlowOutcome::route`) не передаётся — политика по домену назначения. In-memory — `MemOutbound`;
набор `suite::outbound_submit`.

## Identity и EmailSubmission
CTR-30, RFC 8621 §6–7. Адреса отправителя (код — `identity.rs`): `Identity { id, name, email, reply_to, bcc: Vec<IdentityAddress { name?,
email }>, text_signature, html_signature }`, `validate()` (id 1–64 байт без пробелов, имена ≤ 255 символов без управляющих, адреса —
`valid_address`, до 100 в `reply_to`/`bcc`, подписи ≤ 64 КиБ → `Invalid`). `IdentityStore`: `list(scope)` (по id), `get`, `put(scope,
identity) -> state` (создать или заменить целиком), `delete -> state` (нет — `NotFound`), `state(scope)` — растёт при каждом изменении,
у новой учётной записи 0. Адрес входа хранилище не держит — его показывает `tp-jmap` (неудаляемый); право отправки от адреса проверяет
вызывающий (`Authorizer::send_right`).
Учёт отправок (код — `submission.rs`): `EmailSubmission { id, identity, email: MessageId, mail_from, rcpt_to, send_at, undo_status:
UndoStatus::{Pending, Final, Canceled}, delivery: Vec<(String, SubmitStatus)>, queue: Option<QueueId> }`, `validate()`.
`SubmissionStore`: `insert -> state` (занятый id — `Conflict`, `Canceled` — `Invalid`), `get`, `list` (по `send_at`, `id`), `cancel(scope, id)
-> EmailSubmission` (`Pending` → `Canceled`; повтор — как есть; `Final` — `Conflict`, JMAP `cannotUnsend`), `finalize(scope, id, delivery,
queue) -> state` (`Final` со статусами; отменённая — `Conflict`, письмо не ставить в очередь; отмена и фиксация атомарны между собой),
`delete -> state`, `due(now, limit)` — `Pending` с `send_at ≤ now` всех организаций (воркер отложенной отправки), `state(scope)`.
Нет записи — `NotFound`. In-memory — `MemIdentities`, `MemSubmissions`; наборы `suite::{identity_store, submission_store}`. PostgreSQL — `tp-meta`
(карточка meta), переход `tp-jmap` — волна полосы.

## Trash, Retention, Quota (Ф1)
SRS 4.7, 3.8. Код — `retention.rs`. `TrashStore`: `list(scope, offset, limit) -> Vec<TrashItem { scope, object: TrashObject::Message(id), origin, size,
received_at, deleted_at, expires_at }>`, `restore(scope, object) -> Restored::Message { mailbox, inserted }`,
`expired(now, limit)` (все организации, по `expires_at`, включая скрытые), `defer(scope, object, until)`.
«Очистить корзину» (решение 2026-11-01-trash-purge): `purge(scope, ids, &DecisionHooks) -> PurgeOutcome { hidden, erased, modseq }` —
письма, видимые в корзине, по настройке `trash.purge`: `TrashPurge::Hide` (по умолчанию) — скрыть, `Erase` — для каждого
`before_final_delete` вне транзакции: `Allow` — стереть (как `expunge_final`), `Deny` (Legal Hold) или ошибка хука — скрыть;
прочие письма — пропуск. Скрытое письмо не видно пользователю: нет в `MessageStore::get`/`list` и операциях над письмами,
в счётчиках папки, в квоте (`Quota`, `AccountStore::quota`); `ChangeLog` — удалено из корзины, событие ядра — `Trashed`.
Администратор: `list_hidden(scope, offset, limit)`, `restore` (видимые и скрытые; исходная папка или «Входящие», прежний `MessageId`;
протоколы и Admin API — через `MailStore::restore`, с уведомлением клиентов);
`list` — только видимые. Срок корзины для скрытых — как для остальных (`expired` → хук → `expunge_final`).
Настройка: `Policy::trash_purge: Option<TrashPurgeRule { mode, user_can_choose }>` (администратор, уровни цепочки; не задано —
`hide` без выбора пользователя); `PolicyResolver::trash_purge(scope) -> TrashPurgeSetting { rule, user }`,
`effective()` — выбор пользователя только при `user_can_choose`, иначе значение администратора;
`set_user_trash_purge(scope, Option<TrashPurge>)` — без `user_can_choose` выбор `Forbidden`, сброс (`None`) разрешён.
Срок корзины — `PolicyResolver` (`trash_retention`, по умолчанию 30 суток из конфигурации): хранилище вычисляет `expires_at`
при переносе в корзину; изменение политики действует на элементы, удалённые после него.
Сигнал `EventKind::TrashChanged { account }` (системный, `retention::trash_changed(scope)`; CTR-37) — после фиксации `MailStore::delete_to_trash`
и `purge`, если что-то перенесено, скрыто или стёрто; maintenance пересчитывает по нему ближайший срок (`expired`), по `Resync` и при запуске — тоже.
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
`set_blob_store` / `set_directory` / `set_search_index` / `set_secret_store` — замена встроенной реализации; `set_mail_flow` — правила потока
почты (`transport::MailFlow`, раздел Transport). Повтор имени, URI, префикса, реализации — `Conflict`.
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
- 2026-11-01 — CTR-27 (x/contracts-15, решение 2026-11-01-trash-purge): `MailStore::append` — письмо клиента без точек принятия решений
  доставки и копий; реализация `tp-mailstore` — общие с `ingest` шаги разбора, квоты и загрузки.
- 2026-11-01 — решение «Взаимодействие компонентов и требования к СУБД»: сигналы через PostgreSQL LISTEN/NOTIFY, `Rpc`, `AbuseBlocks`,
  `NodeInfo`; канал и шина Valkey удаляются (реализация — CTR-23, CLU-09).
- 2026-11-01 — решение «Состав функций транспорта и внешний MTA»: `transport::{TransportStore, MailFlow}`, `EventKind::TransportChanged`
  (реализация — CTR-24).
- 2026-11-01 — решение «Масштабирование, шарды, геораспределение»: `NodeInfo::site` (CTR-23), `Partition { shard, index }` (CTR-25),
  `ReadIntent` (tp-meta); `MailFlow` — точка расширения модуля, `FlowRule` и хранение правил — в модуле.
- 2026-11-01 — CTR-26 (x/contracts-14, решение 2026-11-01-trash-purge): `retention::{TrashPurge, TrashPurgeRule, TrashPurgeSetting,
  PurgeOutcome}`, `TrashStore::purge`/`list_hidden` (скрытые письма; `list` — без них, `restore`/`defer`/`expired` — и для скрытых),
  `Policy::trash_purge`, `PolicyResolver::trash_purge`/`set_user_trash_purge`; квота без скрытых. In-memory — `MemPolicies`,
  `MemTrash` (скрытые в `MemMetadata`); набор `suite::trash_purge`. PostgreSQL — META-29 (в `PgMeta` пока заглушки).
- 2026-11-04 — решение «Соответствие функциям Exchange» (CTR-35): `MailboxAudit`, `SenderLists`, контакты, динамические группы и оргструктура в `Directory`,
  срок выпуска и отмена в `QueueStore`, выборка журнала доставки по отправке, `AccountState`.
- 2026-11-04 — решение «Внешние встречи» (CTR-36): `AvailabilitySource`, `Registry::add_availability_source`, `CalendarPublications`.
- 2026-11-10 — CTR-23 (x/contracts-12, решение 2026-11-01-interaction-db-ha): `EventKind::{LogAppended, MembersChanged, Blocked, Resync}`,
  `EventKind::is_system`, `publish(Resync)` — `Invalid`; `membership::{NodeInfo, SiteId}`, `Membership::heartbeat(&NodeInfo, ttl)`,
  `live_info()` (`live()` — по умолчанию); `rpc::{Rpc, RpcHandler, RpcService, RpcRequest, RpcError}`; `abuse::{AbuseBlocks, AbuseBlock,
  BlockSubject}`; `InternalChannel::Valkey` удалён (проверка адреса шины Valkey — в `tp-cluster` до CLU-09). In-memory — `MemEventBus`
  (`Resync` при отставании), `MemMembership` (`NodeInfo`, `with_bus`), `MemRpcNet`/`MemRpc`, `MemAbuseBlocks`; наборы `suite::{signals,
  membership_info, rpc, abuse_blocks}`. `PgMeta::live_info` — заглушка до META-27.
- 2026-11-10 — CTR-24 (x/contracts-13, решения 2026-11-01-transport-scope-external-mta и -scale-shards-geo-admin): `transport::{TransportStore,
  ClusterTransport, TenantTransport, AcceptedDomain, DomainKind, TrustedSource, SourceMatch, SourceKind, DestinationPolicy, TlsRequirement,
  IpNet, AddressPattern, normalize_domain, transport_changed}` — записи с версией снимка (устаревшая — `Conflict`); `TrustedSource::tenant`
  (источник приложения — организации); точка расширения `transport::{MailFlow, FlowInput, FlowOutcome, FlowVerdict, FlowDirection,
  FlowEnvelope, BlobRef}`, `Registry::set_mail_flow`/`mail_flow`; `EventKind::TransportChanged { tenant }`. In-memory — `MemTransport`
  (`with_bus`); набор `suite::transport_store`. PostgreSQL — `tp-meta` (отдельная карточка).
- 2026-11-10 — CTR-37 (x/contracts-25): системный сигнал `EventKind::TrashChanged { account }` и `retention::trash_changed(scope)` —
  после `MailStore::delete_to_trash` и `TrashStore::purge` (таймер истечения корзины в кластере, MS-14). In-memory — `MemTrash::with_bus`
  (`purge`); набор `suite::signals`. Публикация в `tp-mailstore` (`delete_to_trash`) и `tp-meta` (`purge`) — волны полос.
- 2026-11-10 — CTR-38 (x/contracts-26): `BlobIndex::next_gc_due` (вместо деления пополам в `tp-blob`); владелец загрузки в учёте —
  `BlobIndex::{mark_upload, upload_owned, prune_uploads}` (вместо учёта в памяти узла `tp-jmap`); `MailStore::restore` — и для скрытых
  писем, с уведомлением (Admin API — не через `TrashStore::restore`). Методы по умолчанию — реализации собираются без правки.
  In-memory — `MemBlobIndex`; наборы `suite::{blob_gc_due, blob_uploads}`. PostgreSQL (`tp-meta`), переход `tp-blob`, `tp-jmap`,
  `tp-mailstore`, `tp-admin` — волны полос.
- 2026-11-10 — CTR-39 (x/contracts-27): системный сигнал `EventKind::Unblocked { subject }` — после `AbuseBlocks::unblock`
  (раньше узлы держали снятую блокировку до срока). In-memory — `MemAbuseBlocks::with_bus`; наборы `suite::abuse_blocks_signals`,
  `suite::signals`; в `suite::abuse_blocks` база сроков округляется вверх до секунды. Публикация в `tp-meta` (`PgMeta::unblock`) —
  волна полосы meta. Пометка «заглушка до META-27» у `Membership` снята.
- 2026-11-10 — CTR-40 (x/contracts-28, отчёт smtp-04): таблица переписывания адресов конверта — данные организации
  (`transport::{RewriteRule, EnvelopeSide}`, `TenantTransport::{rewrites, rewrite, rewrite_rule}`, `TransportStore::put_rewrite`/
  `delete_rewrite` с версией организации; `AddressPattern::{is_domain, domain}`) вместо `smtp.addressing.rewrite`; catch-all —
  псевдоним каталога `@domain` (`DirAlias::address`); `QueueStore::postpone` — перенос получателя без `attempts` + 1. Методы по умолчанию —
  `Internal`. In-memory — `MemTransport`, `MemQueue`; наборы `suite::{transport_rewrites, queue_postpone}`. Хранение в `tp-meta`
  (`PgMeta` — пустая таблица до волны meta), переход `tp-smtp` — волны полос.
- 2026-11-10 — CTR-40: ответ JMAP на `FlowVerdict::Reject` и `Decision::Deny` при отправке — `forbiddenToSend` (RFC 8621, как в JMAP-10),
  а не `forbidden`.
- 2026-11-10 — CTR-28: `queue::Outbound` (`enqueue`, `submit`), `Submitted`, `SubmitStatus` — одно объявление вместо
  `tp_smtp::deliver::Outbound` и `tp_jmap::Outbound`; `tp_smtp::{RcptStatus, Submitted}` (поле `id`) и `tp_jmap::{RcptStatus, Submitted}`
  удалены. `FlowOutcome::route` в контракт не вошёл (очередь его не хранит). In-memory — `MemOutbound`; набор `suite::outbound_submit`.
- 2026-11-11 — CTR-34 (x/contracts-22, решение 2026-11-01-scale-shards-geo-admin): журнал событий по шардам — `ShardId`,
  `events::{ShardPartition, ShardEventLogs, SingleShard, shard_subscriber}`, `PartitionAssigner::assigned_sharded` (по умолчанию; один шард —
  прежнее поведение). `Partition` остался номером раздела шарда (`ShardPartition::index`) — реализации и вызовы собираются без правки.
  In-memory — `MemShardedEventLog`, `MemPartitionAssigner`; наборы `suite::{partition_assigner, shard_event_logs}`. Журналы шардов в `tp-meta`,
  переход подписчиков (`tp-meta::events`, `thepost`) и `LogAppended` с шардом — волны полос.
- 2026-11-11 — CTR-41 (x/contracts-29, решение #193): `admin_token::{LocalAdminTokens, LocalAdminToken, LocalTokenState,
  LOCAL_ADMIN_TOKEN_PREFIX, is_local_admin_token}` — локальный одноразовый токен Admin API (выдача, погашение, срок, отзыв, очистка; хранится
  хэш). In-memory — `MemLocalAdminTokens`; набор `suite::local_admin_tokens`. PostgreSQL (`tp-meta`) и вход в `tp-admin` (ADM-11) — волны полос.
- 2026-11-11 — CTR-29 (x/contracts-17): `MessageStore::move_to` сохраняет `MessageId` (меняются папка, UID, `ModSeq`), как `to_trash`/`restore`;
  `copy` — по-прежнему новый id. Реализации — `PgMeta`, `MemMetadata`; JMAP `Email/set` (`mailboxIds`) — `updated: null`; `tp-mailstore` уведомляет
  исходные папки по состоянию до переноса.
- 2026-11-11 — CTR-30 (x/contracts-18): `identity::{IdentityStore, Identity, IdentityAddress, valid_address}`, `submission::{SubmissionStore,
  EmailSubmission, UndoStatus}` — адреса отправителя и учёт отправок JMAP (статус, отмена до отправки, наступившие отложенные). In-memory —
  `MemIdentities`, `MemSubmissions`; наборы `suite::{identity_store, submission_store}`. Реализация в `tp-meta` и переход `tp-jmap` — волны полос.
- 2026-11-11 — CTR-31 (x/contracts-19): `groups::{Moderation, check_delivery, validate_reject_reason, MAX_REJECT_REASON_LEN}` — решения
  модератора группы рассылки в контракте вместо временного трейта `tp-admin` (ADM-07; `tp_admin::groups::Moderation` — реэкспорт).
  In-memory — `MemModeration`, `ModerationDecision`; набор `suite::moderation`. Реализация в `tp-smtp` — волна полосы.
- 2026-11-11 — CTR-32 (x/contracts-20): `AccountStore::by_external_id` (метод по умолчанию — `Internal`). Реализации — `PgMeta`,
  `MemMetadata`; набор `suite::account_external_id` (переименование входа сохраняет членство в группе). Переход синхронизации групп
  AUTH-07 (`tp-auth`) на сопоставление по `external_id` — волна полосы auth.
- 2026-11-11 — CTR-33 (x/contracts-21, решение 2026-11-01-trash-purge, отчёт imap-05): перенос в корзину (`MessageStore::to_trash`,
  `MailStore::delete_to_trash`, удаление папки с письмами) снимает `\Deleted`. Реализации — `PgMeta` (`relocate`), `MemMetadata`.
- 2026-11-11 — CTR-44 (x/contracts-32, решение 2026-11-11-stage2-followups п. 14): `AccountStore::list(tenant, prefix, after, limit)`
  (метод по умолчанию — `Internal`). Реализации — `PgMeta` (курсор по индексу `accounts_login (tenant_id, lower(login))`, порядок — правило
  сортировки базы по умолчанию), `MemMetadata` (побайтно); набор `suite::account_list`. `tp-admin`: список учётных записей — через
  `AccountStore::list`; временный трейт `AccountList` и `Admin::with_account_list` (ADM-12) удалены.
