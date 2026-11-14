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

### Policy
`meta::Policy` — поля уровня, `None` — не задано (`overlay` накладывает более специфичный уровень). Потребители и умолчания (CTR-67,
решение 97): `quota_bytes`, `trash_retention` — `tp-meta` (умолчание — секция `meta.retention`); `retention` — модуль «Соответствие
требованиям» (удаление по сроку через `DecisionHook::before_final_delete`), ядро не применяет, не задано — без срока; `send_per_hour`
(умолчание `Policy::DEFAULT_SEND_PER_HOUR` = 1000), `rcpts_per_hour` (адресатов за скользящий час, как RecipientRateLimit в Exchange;
`DEFAULT_RCPTS_PER_HOUR` = 2000, решение 113), `max_recipients` (`DEFAULT_MAX_RECIPIENTS` = 100) — лимиты submission в SMTP
(`*_or_default()`); `dkim_selector` — селектор подписи домена в SMTP (не задано — подпись всеми ключами домена); `milter_profile` — набор
фильтров `smtp.milter` (не задано — все фильтры); `trash_purge` — правило корзины (#Trash). Настроек этих полей SMTP в config.yaml нет.
`PolicyTarget::{Tenant, Domain, Account, Mailbox}` — уровень цепочки, один тип на всю систему (CTR-25). `PolicyStore`:
`policy(tenant, target) -> Policy` (уровень без наложения, не задан — `Policy::default()`), `set_policy(tenant, target, &Policy)` (уровень
целиком, `Policy::default()` удаляет уровень); существование объекта уровня проверяет вызывающий, запись сразу видна `PolicyResolver`.
In-memory — `MemPolicies` (`PolicyStore` + `PolicyResolver`); набор `suite::policy_store`. PostgreSQL — META-70, Admin API — ADM-34.
Архив (CTR-76, #Archive): `archive_after` — возраст автопереноса писем в архив (уровень учётной записи: организация → домен → ящик,
уровень папки не применяется; не задано — автопереноса нет), `archive_quota_bytes` — квота архива (не задано — без ограничения);
потребитель — `tp-mailstore` (MS-16), Admin API — поля `PolicyBody`.

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
- `acquire(tenant, owner: Option<AccountId>, hash, refs: u32) -> Result<Acquire>`; `Acquire::Existing { generation }` — ссылки
  добавлены, загрузка не нужна; `Acquire::Absent { generation }` — загрузить по нему и вызвать `commit_new`. `owner` (CTR-49,
  решение 46) — учётная запись, для ящика или отправки которой берутся ссылки: хранилище с шардами выбирает по ней шард учёта;
  `None` — владельца нет или ссылки на ящики нескольких учётных записей. Хранилища без шардов `owner` не используют; `tp-meta`
  со справочником пока ведёт учёт в основной БД (`commit_new`, `release` учётной записи не получают).
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
  сопоставляет по нему, переименование входа не меняет `AccountId` и членство в группах; обязателен — CTR-72),
  `list(tenant, prefix, after, limit)` (CTR-44, список Admin API: по возрастанию `lower(login)` в порядке сравнения строк хранилища,
  префикс и курсор `after` — в нижнем регистре, строго больше; по умолчанию — `Internal`), `upsert` (из LDAP), `quota`.
  Роль администратора установки — явная запись: `grant_installation_admin(tenant, id)` (повтор — не ошибка, нет
  учётной записи — `NotFound`, удаляется с учётной записью) и `is_installation_admin(tenant, id) -> bool`; ставит
  `thepost admin init`, читает вход администратора по паролю (решение 117).
  Псевдонимы (CTR-53) — поле `Account::aliases` (нижний регистр, без повторов, основного адреса и catch-all `@domain`; хранится с
  учётной записью, задаётся `AccountUpsert::aliases` — псевдонимы каталога `DirAlias` с целью — этой учётной записью, заменяют прежние;
  правило хранения — `meta::account_aliases(primary, aliases)`). Адреса, на которые учётной записи доставляется почта, —
  `Account::addresses()`: основной (имя входа) первым, затем псевдонимы, правило — `meta::account_addresses(primary, aliases)`.
  Отдельного метода хранилища нет. Потребитель — DAV-23 (`calendar-user-address-set`).
  Вид учётной записи (CTR-48) — поле `Account::kind: AccountKind::{User, Resource}` (ресурс — переговорная, оборудование), задаётся
  `AccountUpsert::kind` и заменяется при обновлении; правила бронирования ресурса — раздел Sharing.
  Скрыта из книги «Организация» (CTR-83) — поле `Account::hidden_from_gal: bool`, задаётся `AccountUpsert::hidden_from_gal` и
  заменяется при обновлении: нет в GAL, вход и приём почты — как обычно. Синхронизация каталога переносит `DirAccount::hidden_from_gal`.
  Тип ресурса бронирования (CTR-88, решение 121) — поле `Account::resource_type: Option<ResourceType::{Room, Equipment}>` (serde `room`,
  `equipment`; по умолчанию `Room`): у ресурса всегда `Some`, у пользователя — `None`. Задаётся `AccountUpsert::resource_type` и заменяется
  при обновлении по правилу `meta::account_resource_type(kind, requested)`: у ресурса без типа — `Room`, тип у пользователя — `Invalid`,
  смена вида на пользователя снимает тип. Клиентам — CalDAV `CUTYPE` `ROOM`/`RESOURCE` (DAV-38), JMAP `Principal.type`
  `location`/`resource` (JMAP-45); правила бронирования от типа не зависят. In-memory — `MemMetadata`; набор `suite::account_resource_type`.
  `PgMeta` проверяет правило, а хранение типа — META-81 (до неё ресурс читается как `Room`); Admin API — ADM-47.
  Состояние ящика (CTR-35, «Удалённые ящики» решения 2026-11-04-exchange-parity): `AccountState::{Active, Disabled { until }}` —
  отключённый ящик (учётная запись исчезла из каталога): вход и приём почты запрещены независимо от `enabled`, нет в GAL, хранится до
  `until` (по умолчанию 30 дней — политика синхронизации). `account_state` (по умолчанию — через `get`, всегда `Active`),
  `disable_account(tenant, id, until)` (повтор — срок заменяется; `upsert` состояние не меняет), `restore_account`, `disabled_accounts(tenant)
  -> Vec<DisabledAccount { account, until }>` по (`until`, `id`), `expired_accounts(now, limit) -> Vec<AccountScope>` — все организации
  (окончательное удаление через `before_final_delete`), `attach_account(tenant, disabled, to, existing: Option<ExistingContent>)` —
  подключение к учётной записи каталога (Connect-Mailbox: `disabled` получает домен, вход, имя, `external_id`, `enabled` записи `to`,
  запись `to` удаляется; `disabled` действует — `Invalid`, `to` отключена — `Conflict`). Письма ящика `to` — по режиму администратора
  (CTR-50, решение 44): `ExistingContent::Keep` — в подключаемый ящик (системная папка — в папку той же роли, остальные — с иерархией,
  занятое у родителя имя — с суффиксом « 1», « 2»…; корзина и скрытые — со своими сроками), `Delete` — в корзину подключаемого ящика
  со сроком по политике, папки не переносятся; `MessageId` сохраняются, UID новые, ссылки BLOB не меняются; без режима и с письмами
  в `to` — `Invalid` с подсказкой выбрать режим. Перенос — часть операции; долгую операцию Admin API ведёт фоновой задачей (ADM-10).
  `remove_disabled` — после удаления писем вызывающим (только удаляемый, иначе — `Invalid`; остались письма — `Conflict`). Методы,
  кроме `account_state`, обязательны. In-memory — `MemMetadata`; набор `suite::account_state`. PostgreSQL — META-33.
  Удаление (CTR-78): `AccountState::Removing { until }` — окончательное удаление начато; `begin_removal(tenant, id, now)` одной
  операцией с проверкой состояния переводит отключённый ящик со сроком `until ≤ now` в `Removing` (удаляемый — без изменений:
  продолжение прерванного удаления; действующий или с будущим сроком — `Conflict`, нет — `NotFound`). Удаляемый не восстанавливается,
  не подключается и срок не переносится (`restore_account`, `attach_account`, `disable_account` — `Conflict`), остаётся в
  `expired_accounts` и `disabled_accounts`; параллельные `begin_removal` и `restore_account` — успешен ровно один. `tp-mailstore`
  отмечает ящик после разрешения `before_final_delete` и до удаления писем. Набор `suite::lifecycle::account_removal`.
  Сигнал (CTR-77): `EventKind::DisabledMailboxChanged { account }` (системный, `meta::disabled_mailbox_changed(scope)`) — после
  `disable_account` (отключение и смена срока), `restore_account` отключённого и `attach_account`; подписчик — воркер хранения
  отключённых ящиков (MS-17). `MemMetadata::with_bus`; набор `suite::lifecycle::disabled_mailbox_signals`.
  CTR-63: `by_ids(tenant, &[AccountId]) -> Vec<Account>` — в порядке запроса, не найденные и чужой организации пропускаются, повтор —
  одна запись (первое вхождение); обязателен (без реализации по умолчанию; PostgreSQL одним запросом — META-66); набор
  `suite::account_by_ids`.
  CTR-62: `by_address(tenant, address) -> Option<AddressTarget>` — получатель адреса организации (участник приглашения):
  `AddressTarget::Account(Account)` — основной адрес или псевдоним (`Account::addresses`), иначе `AddressTarget::Group(DistributionGroup)`
  — адрес группы рассылки (`GroupStore::by_address`); без учёта регистра и пробелов по краям; вход и состояние ящика не учитываются.
  Обязателен; `MemMetadata` видит группы через `with_groups(MemGroups)`; `PgMeta` до META-68 — основной адрес и адрес группы без
  псевдонимов. Набор `suite::account_by_address`; потребители — DAV-30.
  CTR-64: после фиксации `enabled = false` (`upsert` разрешённой ранее записи) и `disable_account` реализация публикует
  `account_disabled(scope)` — системный сигнал `AccountDisabled { account }` (раздел EventBus; `tp-meta` — META-67).
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
- `acquire::AcquireObserver` (CTR-51, решение 57) — наблюдатель ожидания соединения пула СУБД: хранилище не зависит от крейта метрик,
  а сообщает наблюдателю о каждом ожидании (`pool.acquire()`, `pool.begin()`). Объектно-безопасный, `Send + Sync`; методы `started()`
  (число ожидающих +1) и `finished(AcquireEnd::{Acquired(время), Failed(время), Cancelled})` (−1) — по умолчанию пустые, вызываются
  только сторожем. `observer.wait() -> AcquireWait` — сторож: `acquired()` — «получено», `failed()` — «отказ» (в том числе тайм-аут
  пула), брошенный без исхода (отмена задачи) — `Cancelled`: «ожидает» снимается, время не записывается, отказом не считается.
  `observer.observe(fut)` — то же по результату `Result`. Семантика метрик — как у `tp_platform::dbmetrics::PoolMeter::acquire`
  (число ожидающих, время ожидания, счётчик отказов); отличие — время брошенного ожидания не записывается. `NoAcquireObserver` —
  пустая реализация (хранилище без метрик), `AcquireCounter` — счётчик в памяти для тестов потребителей (`waiting`, `acquired`,
  `failed`, `cancelled`, `observed`, `waited`). Реализация с метриками — PLT-27, вызов в хранилище — META-55. Код — `acquire.rs`.

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
LogAppended { partition } | MembersChanged | Blocked { subject } | Unblocked { subject } | TransportChanged { tenant } | TrashChanged { account } |
ShardsChanged | AccountDisabled { account } | DisabledMailboxChanged { account } | GalChanged { collection, modseq } | DavChanged { kind, collection, modseq } | ScheduleInboxChanged { modseq } | SieveChanged | Resync`. Виды учётной записи (`account = Some`) — `MailboxChanged`,
`MailboxListChanged`, `DavChanged`, `ScheduleInboxChanged`, `SieveChanged`; остальные — системные (`EventKind::is_system`, `account = None`, приходят в `subscribe_system()`; `LogAppended`,
`MembersChanged`, `ShardsChanged` — с `TenantId::DEFAULT`, `Blocked`, `Unblocked` — с организацией блокировки, `TransportChanged` — с организацией или `TenantId::DEFAULT`, `TrashChanged` — с организацией учётной записи). `Resync` вставляет шина в поток подписки (её `tenant`/`account`)
при переполнении буфера подписчика или переподключении слушателя — подписчик сверяется с хранилищем; `publish(Resync)` — `Invalid`.
Реализации: в процессе (`tp-cluster`), PostgreSQL LISTEN/NOTIFY (`tp-meta`, кластер); in-memory — `MemEventBus` (отставание — `Resync`).
Доставка — at-most-once; клиент при запуске, после переподключения и по `Resync` сверяется с хранилищем. Публикация — только после фиксации,
вне транзакции изменения. `MembersChanged` публикует реализация `Membership`, `Blocked`, `Unblocked` — реализация `AbuseBlocks`, `TransportChanged` — `TransportStore`, `TrashChanged` — `MailStore::delete_to_trash` и `TrashStore::purge`, `DavChanged` и `ScheduleInboxChanged` — `DavStore`, `SieveChanged` — `SieveStore`,
`ShardsChanged` (CTR-49, решение 47) — тот, кто меняет справочник шардов (модуль «Шардирование», администрирование): добавлен или выведен
шард, изменились адрес или реплика, учётная запись перенесена; узлы сбрасывают кэш справочника (`tp_meta::shards::ShardDirectory::follow`).
`AccountDisabled` (CTR-64) — `AccountStore` после запрета входа (`enabled = false`, `disable_account`), с организацией учётной записи;
протоколы с долгими сеансами (IMAP-22, JMAP-41) закрывают сеансы учётной записи.
`GalChanged` (CTR-60) — `DavStore::sync_gal`, изменивший книгу GAL, с организацией книги (`dav::gal_changed`).
`QueueReady` (CTR-73, тема `<tenant>/queue/outbound`) — `QueueStore` после `enqueue`, `reschedule`, `postpone`, с организацией записи
(`queue::queue_ready`). `DisabledMailboxChanged` (CTR-77) — `AccountStore`, раздел Metadata.
Набор `suite::signals`.
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
Relay, Dsn, Itip }` (`Itip`, CTR-66 — приём iTIP из письма iMIP: `decision` `Accept` — применено, `Discard` — пропущено, `reason` —
`METHOD` или `METHOD: причина`, `delivery::itip_record(method, &ItipOutcome)`; пишет `tp-smtp`, SMTP-37; `tp-meta` — миграция 0035),
`DeliveryDecision { Accept, Reject, Defer, Quarantine, Discard, Hold }` (строки в нижнем регистре).
`DeliveryLog`: `record(event)`, `record_batch(Vec<_>)` (по умолчанию — по одной), `search(tenant, &DeliveryQuery) -> DeliveryPage
{ events, next: Option<DeliveryCursor> }`, `purge(before)` — по всем организациям (maintenance), можно целыми сутками-секциями.
`by_queue(tenant, QueueId, limit)` (CTR-35) — события записи очереди по `at` (равные — в порядке записи), `limit ≥ 1`: статус доставки
отправки JMAP (`EmailSubmission.deliveryStatus`, связь — `EmailSubmission::queue`); обязателен, набор `suite::delivery_by_queue`.
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
`take_held(tenant, id)` — забрать для решения (повторно — `NotFound`). Страницы очереди (CTR-55):
`held(tenant, Option<GroupId>, after: Option<HeldCursor { held_at, id }>, limit)` — страница строго после курсора в порядке
(`held_at`, `id`) (`None` — с начала; курсор — `HeldCursor::of` последнего письма страницы; страница короче `limit` — последняя),
правило — `groups::held_page`; другого метода чтения очереди нет (потребитель курсора — ADM-21). Синхронизация из каталога — `tp-auth` (AUTH-07), проверки
при приёме — `tp-smtp` (SMTP-16), Admin API — ADM-07; реализация — `tp-meta` (META-21), in-memory — `MemGroups`; набор — `suite::group_store`.
Решения модератора (CTR-31): `Moderation` (вместо временного трейта `tp-admin`) — `deliver(tenant, &group, &HeldMessage)` доставляет
одобренное письмо участникам группы (раскрытие `expand`, приём в ящики, журнал доставки; другая группа или организация — `Invalid`,
проверка — `check_delivery`), `reject(tenant, &HeldMessage, reason)` — DSN отправителю (`validate_reject_reason`: одна строка
1–500 символов, иначе `Invalid`; пустой отправитель — без DSN). Письмо уже забрано `take_held`; успех освобождает ссылку на объект,
ошибка — вызывающий возвращает письмо `hold`. Реализация — `tp-smtp` (карточка smtp), in-memory — `MemModeration` (запись решений,
`fail_next`); набор `suite::moderation`.
Одна очередь модерации для групп и правил потока (CTR-43, решение 2026-11-11-stage2-followups п. 9): `HeldMessage::group: Option<GroupId>`
(`None` — письмо задержано правилом), `source: HoldSource::{Group, FlowRule { rule }}`, `moderators: Vec<Moderator::{Account, Group}>`
(у правила — не пусто; у группы — снимок или пусто = модераторы группы), `recipients` — исходные получатели (у правила — не пусто);
новые поля — `#[serde(default)]`; `HeldMessage::validate()` (источник и группа согласованы, имя правила 1–255 символов) — в `hold`
(`Invalid`). `Moderation::deliver(tenant, group: Option<&DistributionGroup>, message)`: без группы — доставка исходным получателям;
`check_delivery` — письмо группы без группы или правила с группой — `Invalid`. `ModerationDecision::Deliver(HeldId, Option<GroupId>)`; набор
`suite::rule_moderation` (задержка правилом, одобрение — доставка получателям, отказ — DSN). Admin API: очередь модератора показывает
`source`, `rule`, `moderators`, `recipients`, одобрение письма правила — `deliver(None)`. `PgMeta`: письма групп — как прежде (снимок
модераторов и получателей не хранится), письмо правила — `Internal` до META-44 (колонка группы допускает NULL, модераторы, источник).
Постановка правилом (`FlowVerdict::Moderate` → очередь вместо 451, имя правила — из `FlowOutcome::matched`) — SMTP-30.

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
CTR-35: `DirAccount::org: DirOrg { department (external_id подразделения), title, manager (external_id), seniority }` (`#[serde(default)]`);
`contacts_since(tenant, cursor, limit) -> DirPage<DirContact { external_id, display_name, email, attributes }>` (класс и ветка — настройка;
без реализации по умолчанию, CTR-87: контакты отдают все каталоги — LDAP, `PgMeta`, `FakeDirectory`), `units(tenant) -> Vec<DirUnit { external_id, name, parent }>` (по умолчанию — пусто);
`DirGroup::members` — также `external_id` контактов. In-memory — `FakeDirectory` (`upsert_contact`, `remove_contact`, `set_units`).
Порог массового удаления: `exceeds_removal_threshold(removed, total, percent)`, `MASS_REMOVAL_PERCENT` = 5; `SyncGate` — остановка
синхронизации до подтверждения: `hold_sync(tenant, removed, total, at) -> MassRemoval { removed, total, detected_at, approved }` (подтверждение
сохраняется, если `removed` не выросло), `pending_sync`, `approve_sync` (нет — `NotFound`; Admin API), `clear_sync` (синхронизация после
отключения). PostgreSQL — `PgMeta` (`sync_holds`), in-memory — `MemSyncGate`; набор `suite::sync_gate`.
CTR-58 (решение 79): `DirAccount::hidden_from_gal: bool` (`#[serde(default)]`, как у групп и почтовых контактов) — запись не попадает
в адресную книгу (GAL), вход и приём почты не меняются. Заполняет каталог (LDAP — AUTH-16), учитывает построение GAL (DAV-27).
Каталог PostgreSQL (узел без LDAP, `impl Directory for PgMeta`, META-65) отдаёт признак учётной записи `Account::hidden_from_gal`
(CTR-83): запись остаётся в `accounts_since` с признаком, как из LDAP.

## AddressBook
SRS 4.5, 5.1, решение 2026-11-04-exchange-parity (CTR-35; код — `addressbook.rs`). Ids — `ContactId`, `OrgUnitId`. Состояние учётной записи
не учитывается: отключённые ящики исключает вызывающий.
- `ContactStore`: `MailContact { id, tenant, display_name, email, external_id: Option<_> (None — Admin API), attributes, groups: Vec<GroupId>,
  hidden_from_gal }`, `validate()`; `contact`, `contact_by_address` (без учёта регистра), `contact_by_external_id`, `contacts(tenant)` (по `id`),
  `put_contact` (замена целиком, `groups` — по возрастанию без повторов; занятый адрес или `external_id` — `Conflict`), `remove_contact`
  (нет — `NotFound`), `group_contacts(tenant, group)` — внешние получатели при раскрытии группы.
- `OrgStore`: `OrgUnit { id, tenant, name, parent, external_id }`, `check_unit` (неизвестный родитель, цикл — `Invalid`), `unit_path` (от корня —
  `ORG` в vCard); `org_units`, `put_org_unit` (занятый `external_id` — `Conflict`), `remove_org_unit` (есть вложенные — `Conflict`; у учётных
  записей `department` сбрасывается). `OrgAttributes { department, title, manager, seniority, attributes }` (`validate()`,
  `filter_attributes()` — с `title`); `org` (не заданы — по умолчанию), `set_org` (неизвестное подразделение, руководитель — сама запись —
  `Invalid`; значение по умолчанию удаляет запись), `org_members(tenant, unit?, subtree) -> Vec<OrgMember>` в порядке `sort_members`
  (старшинство по убыванию, без него — в конце, затем `AccountId`; неизвестное подразделение — `NotFound`).
- `DynamicGroupStore`: динамическая группа — `DistributionGroup` в `GroupStore` (адрес, отправители, модерация, GAL) без статических членов и
  `DynamicGroup { group, tenant, filter: MemberFilter, contacts }`. `MemberFilter::{Equals, Prefix, Present, Unit { unit, subtree }, All, Any,
  Not}` — атрибуты без учёта регистра ASCII, значения — без учёта регистра; `validate()` (≤ 64 узлов, глубина ≤ 8), `matches(FilterTarget
  { attributes, units })`. `dynamic`, `dynamic_groups`, `put_dynamic` (`Invalid`; PostgreSQL может требовать группу — `NotFound`),
  `remove_dynamic`, `expand_dynamic -> Members { accounts, contacts }` (по возрастанию; учётные записи с оргатрибутами и, если `contacts`, —
  контакты; не динамическая — `NotFound`).
In-memory — `MemAddressBook`; наборы `suite::{contact_store, org_store, dynamic_groups}`. PostgreSQL — META-33, синхронизация — AUTH-10,
Admin API — ADM-10, доставка — SMTP-26, JMAP `urn:thepost:orgchart` — JMAP-17.

## SenderLists
SRS 4.1, решение 2026-11-04-exchange-parity (CTR-35; код — `senders.rs`). Личные списки ящика поверх вердикта антиспама: заблокированный —
«Спам», надёжный — «Входящие» (SMTP-25). `SenderList { safe, blocked }`: записи — адрес или домен (`@domain` на входе), `normalize_entry`
(нижний регистр ASCII), `normalized()` (по возрастанию без повторов, до `MAX_SENDER_ENTRIES` = 1024 в каждом; иначе `Invalid`),
`verdict(from) -> Option<SenderVerdict::{Safe, Blocked}>` — адрес целиком, затем домен (только точное совпадение), на одном уровне надёжный
приоритетнее. `SenderLists`: `sender_list(scope)` (нет — пусто), `put_sender_list(scope, list) -> state` (замена целиком), `sender_list_state`
(у нового ящика 0), `sender_verdict` (по умолчанию — через `sender_list`). In-memory — `MemSenderLists`; набор `suite::sender_lists`.
JMAP `urn:thepost:senderlists` — JMAP-17, Admin API — ADM-10, PostgreSQL — META-33.

## MailboxAudit
SRS 5.2, решение 2026-11-04-exchange-parity (CTR-35; код — `audit.rs`). `AuditEntry { scope (ящик), at, actor (вход или субъект OIDC),
actor_account: Option<AccountId>, actor_kind: AuditActorKind::{Owner, Delegate, Admin}, action: AuditAction::{Login, Read, Search, Move,
Delete, Send, SendAs, SendOnBehalf, ChangeRights, ChangeRules, ChangeForwarding}, object: AuditObject { mailbox?, message?, detail? }, client? }`,
`validate()` (у владельца `actor_account` — ящик). `Search` (решение 114, CTR-81) — поиск по тексту писем (IMAP `SEARCH`/`UID SEARCH` с
`BODY`/`TEXT`, JMAP `Email/query` с `text`/`body`): строка запроса — в `detail`, папка — в `mailbox`, найденные письма не записываются.
`AuditPolicy { non_owner (по умолчанию true), owner: BTreeSet<AuditAction> (по умолчанию пусто; только `AuditAction::OWNER`) }`,
`records(kind, action)` — вход, чтение и поиск владельцем не записываются никогда. `MailboxAudit`:
`record_audit(entry) -> bool` — протоколы вызывают для каждого действия, политику применяет хранилище; `audit_policy(scope)` (ящик → организация
→ по умолчанию), `tenant_audit_policy(tenant) -> Option<AuditPolicy>` (заданная политика организации, ADM-40), `set_audit_policy(tenant,
account?, policy?)`, `search_audit(tenant, &AuditQuery { account, actor, actor_kind, action, since,
until, limit, after })` -> `AuditPage` (как `DeliveryLog::search`), `purge_audit(before)` (maintenance, можно сутками). In-memory —
`MemMailboxAudit` (`entries`, `fail_next`); набор `suite::mailbox_audit`. Запись — IMAP-15, JMAP-18; PostgreSQL (только добавление) — META-33;
выгрузка в SIEM и выборка — ADM-10.

## AdminAuditLog
SRS 5.2, 6.8, решение 98 (CTR-25; код — `admin_audit.rs`). Журнал аудита Admin API, только добавление. `AdminAuditRecord { at, tenant?
(`None` — уровень инсталляции), actor_tenant, actor, actor_login, roles: Vec<String> (роли Admin API), method, route, path, status,
content_messages? }` — без тела запроса и секретов. `AuditLog`: `append(&record) -> seq` (номер ≥ 1, растёт в порядке добавления);
`search(tenant, &AdminAuditQuery { since?, until?, actor?, method?, route_prefix?, limit ≥ 1, after? }) -> AdminAuditPage { entries:
Vec<AdminAuditEntry { seq, record }>, next? }` — только записи организации (без уровня инсталляции), по возрастанию `seq`, условия по «и»
(`AdminAuditQuery::matches`: `at` в [`since`, `until`), `actor` — имя входа без учёта регистра или UUID, `method` — точно, `route_prefix` —
начало шаблона маршрута; ADM-30) отбирает хранилище, `next` — у полной страницы; `AdminAuditQuery::first(limit)` — без условий;
`AdminAuditQuery::validate` (неверный, в том числе пустое условие, — `Invalid`). Выгрузка в SIEM (решение владельца 12.11: база + выгрузка из неё, APP-59):
`export_page(after, limit ≥ 1) -> Vec<AdminAuditEntry>` — все записи (и уровень инсталляции) с `seq > after` по возрастанию; видимые записи —
непрерывный префикс порядка добавления (меньший номер не появляется после бо́льшего); `export_mark(sink) -> seq` (0 — ничего не доставлено),
`advance_export_mark(sink, from, to) -> bool` — сдвиг, только если отметка всё ещё `from` (`to > from`, иначе `Invalid`; `check_export_mark`).
In-memory — `MemAuditLog` (`entries`); набор `suite::admin_audit_log`. PostgreSQL (таблицы `audit_log`, META-22, и `audit_export`, APP-59) —
META-70; запись и `GET …/audit` — Admin API (ADM-34); выгрузка — роль maintenance узла под лизой (APP-59).

## Auth
SRS 5.1. `Authenticator`: `verify(Credentials, peer: Option<IpAddr>) -> Result<Principal>` (неверные данные — `Forbidden`);
`Credentials::{ Password { login, secret }, Bearer(token), Gssapi { principal, authzid } }`, секреты скрыты в `Debug`.
`Principal { tenant, account, login, groups, scopes: Option<_> }` (`None` — без ограничений).
Адрес клиента (CTR-57; SRS 6.3 — счётчики попыток по адресу, подсети и учётной записи) — обязательный параметр единственного метода
проверки `verify(credentials, peer)`: реализация с защитой от перебора ведёт по нему счётчики и отказывает заблокированному источнику
(`Forbidden`); `None` — только когда адрес неизвестен (вызов без соединения клиента), счётчики по адресу и подсети тогда не ведутся.
Протоколы передают адрес соединения (`ConnInfo::client` — из заголовка PROXY или адрес TCP-собеседника), HTTP (Basic, Bearer: `tp-dav`,
`tp-jmap`, `tp-admin`) — адрес из расширения запроса `ConnectInfo<SocketAddr>`, которое ставит `tp_platform::service_http`.
`Authorizer`: `rights(&Principal, &Resource) -> Rights`, `check(.., required)` (по умолчанию через `rights`),
`send_right(&Principal, from) -> Option<SendRight::{SendAs, SendOnBehalf}>`; `Resource::{ Account, Mailbox }`. Права на календари и
адресные книги — `SharingAuthorizer` (раздел Sharing).
`Rights` — RFC 4314, строка `lrswipkxtea` в каноническом порядке; `c`/`d` (RFC 2086) разворачивает IMAP.
SASL (код — `sasl.rs`, RFC 4422): механизмы `Mechanism::{ Plain, Login, OauthBearer, Xoauth2, Gssapi }` (`name()`, разбор без учёта регистра,
`plaintext()` — без TLS не предлагается) — `tp-auth` поверх `Authenticator`; `SaslServer { mechanisms(tls) -> Vec<Mechanism>,
start(mech, &SaslContext { service, tls, peer }) }` → `SaslSession::step(Option<&[u8]>) -> SaslStep::{ Challenge(bytes), Done(Principal), Fail(SaslFailure) }`
(`None` — нет начального ответа; `SaslFailure::{ Authentication, Authorization, Expired }` — коды RFC 5530 / SMTP 535).
Неизвестный или недоступный механизм, нарушение формата — `Invalid`; base64 и отмена `*` — протокол.
`SaslContext::peer: Option<IpAddr>` (CTR-57) — адрес клиента соединения: сеанс передаёт его в `Authenticator::verify`; `None` — только
когда адрес неизвестен. Начало обмена одно — `start`; smtp, imap, pop3 и managesieve собирают `SaslContext` с адресом соединения.
Фейки `tp-testkit`: `FakeSasl` передаёт адрес, `FakeAuthenticator::peers()` возвращает адреса всех вызовов.
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
`Unavailable` — недоступно или запечатано), `put(&SecretRef, &Secret) -> Result<()>` — новая версия поля, прочие поля
пути сохраняются (`Forbidden` — политика, `Unavailable` — недоступно, `Invalid` — не настроено, путь не для записи или
изменён параллельно; реализации по умолчанию нет), `renew(&SecretLease) -> Result<SecretLease>` (неизвестная/истёкшая — `NotFound`, перечитать;
по умолчанию — `NotFound`). Встроенный клиент — `tp-platform` (PLT-12), замена — `Registry::set_secret_store`, in-memory — `tp-testkit`.
Код — `secrets.rs`.

## Internal TLS
SRS 6.2, решение 2026-10-30. Узел ↔ PostgreSQL, S3, узел ↔ узел — только TLS. Общая секция `tls::InternalTls
{ ca_file, cert_file, key_file, key, server_name, allow_plaintext }` — поле `tls` секции канала; `Profile { Prod (по умолчанию), Dev }` —
параметр `profile` верхнего уровня config.yaml. `InternalTls::check_url(InternalChannel::{Postgres, S3, Node}, url, profile)
-> Result<Transport::{Tls, Plaintext}>` вызывается в `thepost config check` и при запуске: `allow_plaintext` вне dev, адрес без TLS
без `allow_plaintext`, `sslmode=require|verify-ca` — `Invalid`. Схемы: PostgreSQL — `sslmode=verify-full` (без `sslmode` клиент
обязан verify-full), S3 — `https://`, узел — `https://`/`smtps://`. `Transport::Plaintext` — крейт канала пишет
`InternalChannel::plaintext_warning()` в лог на каждом запуске. Служебный HTTP — TLS слушателя (`tp-platform`).
Ключ клиентского сертификата (CTR-52, решение 37) — из хранилища секретов: `key: { secret: "<путь>#<поле>" }` (`SecretRef`, значение — PEM)
вместо `key_file`; `key_file` остаётся для установок без OpenBao. `key` и `key_file` вместе — `Invalid`; `cert_file` задаётся вместе
с `key_file` или `key`; ключ открытым текстом в `key` — ошибка разбора «secret in plaintext» без значения в тексте.
`InternalTls::key_source() -> Result<Option<KeySource::{File(&Path), Secret(&SecretRef)}>>` — источник ключа; крейты каналов берут ключ
отсюда, а не из полей, секрет читают сами через `SecretStore::get` (межузловой канал — CLU-16, клиент к СУБД — META-56,
подключение в узле — APP-36). Смена ссылки применяется при новом подключении.

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
`enqueue(owner: Option<AccountId>, OutMessage { tenant, blob, size, from, dsn: DsnParams { envid, ret }, rcpts: Vec<OutRcpt { address, notify, orcpt }>, next_attempt }) -> QueueId`
(пусто или повтор адреса — `Invalid`; CTR-49, решение 46: `owner` — учётная запись отправителя, хранилище с шардами ставит запись на шард её ящика (`tp-meta`: неизвестная или `None` — основная
БД), без шардов не используется; её передаёт `Outbound`, CTR-74); `claim(now, limit, lease) -> Vec<Claimed { claim: QueueClaim { id, fencing }, until, queued_at, .., rcpts: Vec<QueuedRcpt { rcpt, attempts, last_error }> }>`
— записи с наступившим сроком попытки без действующей лизы; `renew(&claim, until)`, `reschedule(&claim, rcpt, at, reason)` (`attempts` + 1),
`postpone(&claim, rcpt, at)` (перенос планировщиком без попытки — лимиты домена или узла: `attempts`, `last_error` не меняются; по
умолчанию — `Internal`; набор `suite::queue_postpone`),
`complete(&claim, rcpt, RcptStatus::{ Delivered { reply }, Failed { reply } }) -> bool` (`true` — все получатели завершены, запись удалена).
Срок выпуска (CTR-35): `OutMessage::next_attempt` (`release_at()`) — для отложенной отправки срок `sendAt`; `Claimed::queued_at` — не раньше
срока выпуска (от него считается срок доставки). `cancel_queued(tenant, id) -> BlobKey` — отмена до первого захвата (запись удаляется,
ссылку освобождает вызывающий; захвачена — `Conflict` = `cannotUnsend`, нет — `NotFound`; атомарна с `claim`; по умолчанию — `Internal`);
набор `suite::queue_release`. Выпуск по таймеру ближайшего срока — SMTP-24, PostgreSQL — META-33.
Без опроса (CTR-73, SRS 2.8): `next_due() -> Option<OffsetDateTime>` — ближайший момент, когда `claim` выдаст запись (наименьший срок
попытки незавершённых получателей, у записи под лизой — не раньше конца лизы; все организации; пусто — `None`); после фиксации
`enqueue`, `reschedule`, `postpone`, `retry` — сигнал `QueueReady` организации записи (`queue::queue_ready`). smtp-out ждёт сигнала или срока
`next_due` (SMTP-42). Обязателен; `MemQueue::with_bus`; набор `suite::lifecycle::queue_next_due`.
Просмотр и повтор (CTR-85; консоль и TUI, раздел «Очередь»): `list(tenant, &QueueFilter { state, address }, after: Option<QueueId>, limit,
now) -> Vec<QueueItem { id, state, queued_at, next_attempt, from, size, rcpts: Vec<QueueItemRcpt { rcpt, state, attempts, last_error }> }>`
— записи организации по `id` строго после `after`, не более `limit` (порция короче — последняя; `limit` = 0 — `Invalid`); состояние
в момент `now` — `QueueState::{Ready, Retry, Active}` (`Active` — действующая лиза, `Retry` — у незавершённого получателя `attempts` > 0);
получатель — `QueueRcptState::{Pending { next_attempt }, Done(RcptStatus)}`; `next_attempt` записи — наименьший у незавершённых.
Фильтр — `QueueFilter::matches` (состояние; подстрока адреса отправителя или получателя без учёта регистра). `retry(tenant, id, now) -> bool`
— срок попытки `now` у незавершённых получателей (`attempts` не меняется; отложенное письмо выпускается, `queued_at` не позже `now`),
после фиксации — `QueueReady`; под действующей лизой — `false` без изменений; нет записи (другой организации, завершена) — `NotFound`.
Оба по умолчанию — `Internal`; in-memory — `MemQueue`; набор `suite::lifecycle::queue_list`. PostgreSQL — отдельная волна meta.
Устаревший захват — `Conflict`; не завершённые до истечения лизы получатели возвращаются в очередь без `attempts` + 1.
Тело — один объект BLOB: ссылку учитывает вызывающий до `enqueue` и освобождает после `complete` = `true`.
Расписание повторов, срок доставки и DSN — `tp-smtp` (DSN ставится в очередь до `complete(.., Failed)`).
Постановка письма протоколами — `Outbound` (CTR-28): `enqueue(tenant, owner: Option<AccountId>, mail_from, rcpts, raw: Bytes)`;
`submit(..) -> Submitted { rcpts: Vec<(String, SubmitStatus::{ Delivered, Queued, Failed(reply) })> }` — статус по получателю в порядке
запроса (по умолчанию — `enqueue`, все `Queued`). Реализации — `tp_smtp::queue::Queue` (тело в BLOB, ссылка, подпись DKIM `with_dkim`),
`tp_smtp::Delivery` (локальные — сразу в ящики); потребители — submission SMTP, пересылки модулей, JMAP `EmailSubmission/set`.
Маршрут правила потока (`FlowOutcome::route`) не передаётся — политика по домену назначения. In-memory — `MemOutbound`;
набор `suite::outbound_submit`. `owner` (CTR-74, решение 46) — учётная запись отправителя у `enqueue`, `submit`, `schedule`: реализация
передаёт её в `BlobIndex::acquire` и `QueueStore::enqueue` (шард её ящика); `None` — DSN, пересылка, письмо модуля. Submission SMTP —
вошедшая учётная запись, JMAP и iTIP — учётная запись отправки, `tp_smtp::Delivery` для локальной отправки — владелец адреса отправителя
конверта. `schedule` обязателен.

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
Отложенная отправка и отмена (CTR-42, решение владельца #195): письмо с `sendAt` в будущем (предел — `maxDelayedSend` конфигурации
`tp-jmap`) ставится в очередь сразу со сроком выпуска — `Outbound::schedule(tenant, owner, mail_from, rcpts, raw, release_at) -> QueueId` (все
получатели, локальные — при выпуске), отправка — `Pending` с `queue`; отмена — `Outbound::cancel_scheduled(tenant, queue)` (`QueueStore::cancel_queued`
и освобождение тела; выпущено — `Conflict` = `cannotUnsend`, нет — `NotFound`), затем `SubmissionStore::cancel`; наступившие (`due`) — `finalize`;
статусы — `DeliveryLog::by_queue`. `schedule` обязателен (CTR-74), `cancel_scheduled` по умолчанию — `Internal`; in-memory — `MemOutbound` (`scheduled`, `release`), набор
`suite::outbound_schedule`. `SubmissionStore` (CTR-42): `query_submissions(scope, &SubmissionFilter { identity_ids, email_ids, undo_status, before,
after })` (RFC 8621 §7.3; `threadIds` вызывающий переводит в `email_ids`; по `send_at`, `id`; по умолчанию — `list` с фильтром),
`submission_changes(scope, since, limit) -> SubmissionChanges { created, updated, destroyed, new_state, has_more }` (созданные и удалённые после
`since` не входят; неизвестное или устаревшее состояние — `Conflict` = `cannotCalculateChanges`; `limit = 0` — `Invalid`), `expire_submissions(before,
limit) -> usize` — срок хранения: `Final` и `Canceled` с `send_at < before` всех организаций (maintenance; срок — конфигурация); оба
обязательны. Набор `suite::submission_query`. Далее: meta (таблица, RLS), smtp (выпуск по сроку, отмена), jmap (`maxDelayedSend`,
`EmailSubmission/get|set|query|queryChanges`).

## Trash, Retention, Quota (Ф1)
SRS 4.7, 3.8. Код — `retention.rs`. `TrashStore`: `list(scope, offset, limit) -> Vec<TrashItem { scope, object: TrashObject::Message(id), origin, size,
received_at, deleted_at, expires_at }>`, `restore(scope, object) -> Restored::Message { mailbox, inserted }`,
`expired(now, limit)` (все организации, по `expires_at`, включая скрытые), `defer(scope, object, until)`.
«Очистить корзину» (решение 2026-11-01-trash-purge): `purge(scope, ids, &DecisionHooks) -> PurgeOutcome { hidden, erased, modseq }` —
письма, видимые в корзине, по настройке `trash.purge`: `TrashPurge::Hide` (по умолчанию) — скрыть, `Erase` — для каждого
`before_final_delete` вне транзакции: `Allow` — стереть (как `expunge_final`), `Deny` (Legal Hold) или ошибка хука — скрыть;
прочие письма — пропуск. Скрытое письмо не видно пользователю: нет в `MessageStore::get`/`list` и операциях над письмами,
в счётчиках папки, в квоте (`Quota`, `AccountStore::quota`); `ChangeLog` — удалено из корзины, событие ядра — `Trashed`.
Администратор: `list_hidden(scope, offset, limit)`, `hidden_message(scope, id) -> Option<Message>` (скрытое письмо — метаданные как у
`MessageStore::get`, тема/отправитель/дата — в `meta.envelope`; скрытие не снимается; нет или не скрыто — `None`; аудит — у вызывающего),
`restore` (видимые и скрытые; исходная папка или «Входящие», прежний `MessageId`;
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
`Quota`: `usage(scope) -> QuotaUsage` (основная квота: письма вне ветки архива, включая корзину, без скрытых; лимит —
`PolicyResolver::quota_bytes`), `check(scope, bytes)` → `QuotaExceeded` (по умолчанию через `usage` и `QuotaUsage::allows`).
`AccountStore::quota` — то же использование без лимита.

### Archive
CTR-76, SRS 4.7. Код — `archive.rs`. Архив — ветка папок учётной записи: папка с ролью `SpecialUse::Archive` (RFC 6154) и вложенные.
Квоты раздельные: письма ветки — в квоте архива, остальные — в основной (`Quota`), перенос в архив освобождает основную квоту.
`ArchiveStore`: `archive_due(now, limit) -> Vec<ArchiveItem { scope, message, mailbox, due }>` — письма всех организаций вне ветки архива
и корзины, без скрытых, у действующих ящиков с заданным `Policy::archive_after`, со сроком `due = received_at + archive_after` (политика
на момент запроса) не позже `now`, по (`due`, `message`); ближайший срок — первый элемент выборки с моментом в далёком будущем и
`limit = 1` (один таймер, без опроса); `archive_usage(scope) -> QuotaUsage` — письма ветки архива, лимит — `Policy::archive_quota_bytes`
(нет учётной записи — `NotFound`). Перенос — `MessageStore::move_to`, квоту архива проверяет вызывающий. Реализации — `PgMeta`
(миграция 0036), in-memory — `MemArchive` (`MemMetadata` + `MemPolicies`); набор `suite::lifecycle::archive`. Потребитель — MS-16.
Политики хранения и Legal Hold — модуль «Соответствие требованиям» через `DecisionHook::before_final_delete`.

## Dav
SRS 4.4, 3.1; CTR-45. Код — `dav.rs`. Одно хранилище для CalDAV, CardDAV и JMAP Calendars/Contacts; тело объекта хранилище не разбирает —
индекс вычисляет вызывающий (`tp_dav::index` поверх calcard). JMAP Calendars — по текущему черновику IETF (решение 35).
`CollectionId`, `DavObjectId` (UUID v7, `ids.rs`); `CollectionKind::{Calendar, AddressBook}`; `Component::{Event, Todo, Journal, Card}`
(`VEVENT`… `VCARD`, `fits(kind)`). `Collection { id, kind, name (сегмент URL, `valid_name`), display_name, description, color, order,
timezone (IANA), components: BTreeSet, transparent (CTR-47: `schedule-calendar-transp`, не входит в занятость), state: ModSeq }`,
`Collection::new(kind, name)`, `validate()` (→ `Invalid`); `kind` и `name` не меняются.
`DavObject { id, collection, name, etag (без кавычек, `valid_etag`), body: String, index: ObjectIndex { component, uid, span: Option<Span {
start, end: Option }>, busy: Vec<BusyPeriod>, schedule_tag: Option<String> } }` — интервал с развёрнутыми повторами, `end = None` —
бесконечный; `span = None` — без дат (кандидат любого запроса); `busy` и `schedule_tag` — раздел Schedule (CTR-47, `#[serde(default)]`);
`ObjectIndex::new(component, uid, span)` — без них.
`TimeRange { start?, end? }`, `ObjectIndex::may_overlap` — кандидат time-range: `start < range.end`, `end ≥ range.start` (точную проверку
RFC 4791 §9.9 делает вызывающий).
`DavStore` (по `AccountScope`, чужая учётная запись или организация — как отсутствующие): `limits() -> DavLimits { max_object_size,
max_objects (в коллекции), max_collections (учётной записи), max_busy_instances (META-54) }` (из конфигурации реализации; превышение — `QuotaExceeded`);
коллекции — `list_collections(kind)` (по `order`, имени), `get_collection(id)`, `collection_by_name(kind, name)`, `create_collection`
(занятые `id` или имя вида — `Conflict`), `update_collection` (свойства), `delete_collection` (сразу, без корзины; объекты и коллекция —
в надгробия, каждый со своим `ModSeq`); объекты — `get_object(collection, name)`, `object_by_uid`, `list_objects -> Vec<ObjectRef { id, name,
etag }>`, `multiget(names)`, `objects_by_id(ids)` (JMAP), `in_range(collection, TimeRange)`, `put_object(collection, name, body, index,
Precondition::{Any, IfMatch(etag), IfNoneMatch}) -> PutOutcome::{Stored(Put { id, etag, modseq, created }), PreconditionFailed,
UidConflict { name }}` (UID уникален в коллекции; компонент не из `components` — `Invalid`; перезапись сохраняет `id`),
`delete_object(collection, name, Precondition) -> DeleteOutcome::{Deleted(ModSeq), PreconditionFailed}` (нет ресурса при `Any` — `NotFound`).
Свойства коллекции (CTR-56, решение 82) — поля `Collection` (`#[serde(default)]`): `is_default`, `subscribed`, `visible`,
`alerts_with_time`, `alerts_without_time`; `Collection::new` — не по умолчанию, подписана, видима, без оповещений. `is_default` —
коллекция по умолчанию вида (JMAP `isDefault`, `schedule-default-calendar-URL`), не больше одной на вид; `subscribed`, `visible` — JMAP
`isSubscribed`, `isVisible`; оповещения по умолчанию для событий со временем и на весь день (JMAP `defaultAlertsWithTime`,
`defaultAlertsWithoutTime`; CalDAV `default-alarm-vevent-datetime`, `default-alarm-vevent-date`) — текст JSON-объекта `Id[Alert]`
JSCalendar до `MAX_DEFAULT_ALERTS_LEN` = 8192 байт, только у календаря (`Collection::validate` → `Invalid`); хранилище его не
разбирает, в `VALARM` переводит `tp-dav`. Читаются с коллекцией (`get_collection`, `list_collections`), меняются обычной записью —
`create_collection`, `update_collection`; отдельных методов нет. Запись с `is_default = true` снимает признак с прежней коллекции по
умолчанию того же вида той же транзакцией: у прежней — своё изменение свойств (`ModSeq`, `state`, `collection_changes`, `DavChanged`)
раньше изменения записываемой; `false` у текущей — не назначена. Коллекция по умолчанию среди списка — `dav::default_of(&[Collection])`:
назначенная, иначе сегмент `default` (`DEFAULT_COLLECTION_NAME`), иначе первая по (`order`, имя). Потребители записи свойств — DAV-25,
JMAP-36.
Перенос (CTR-54, решение 80): `move_object(scope, from, name, to, Precondition) -> MoveOutcome::{Moved(Moved { id, etag, removed, modseq }),
PreconditionFailed, UidConflict { name }}` — одной транзакцией; имя, `id`, тело, индекс и поисковые строки сохраняются, ETag и `ModSeq` —
новые: в `from` — надгробие (`removed`, видно в `changes`), в `to` — создание (`modseq`), в `account_changes` объект — в `updated`;
сигнал `DavChanged` — по обеим коллекциям. `from == to`, разный вид коллекций или компонент не из `components` целевой — `Invalid`;
условие — к исходному ресурсу (`IfNoneMatch` — как `Any`); имя занято в целевой — `PreconditionFailed`, UID — `UidConflict`;
`max_objects` целевой — `QuotaExceeded`.
Поиск контактов (CTR-54): `ObjectIndex::search: CardSearch { strings }` (`#[serde(default)]`) — поисковые строки контакта (полное
имя, части имени, адреса e-mail; нижний регистр, без повторов и управляющих символов, до `MAX_CARD_SEARCH_STRINGS` = 64 строк по
`MAX_CARD_SEARCH_LEN` = 255 байт; `CardSearch::new(values)` приводит и обрезает, `validate()`, `matches(needle)`), вычисляет вызывающий
(`tp_dav::index::card`); непустые — только у `VCARD` (`ObjectIndex::validate` → `Invalid`). Пишутся единственным методом записи
`put_object` и заменяются при каждой записи, читаются с объектом. `find_cards(scope, collection, needle, limit) -> Vec<DavObject>` —
кандидаты, у которых строка содержит `needle` (пустой — все), по имени, не более `limit`; точную проверку фильтра делает вызывающий
(как после `in_range`). Потребители `find_cards` — DAV-24, JMAP-34.
Изменения: `ModSeq` — из счётчика учётной записи (как у писем), у каждого изменения свой; `Collection::state` — наибольший `ModSeq`
коллекции (CTag). `changes(collection, since, limit) -> Changes<String>` (имена, RFC 6578; повторно созданное имя — не в `destroyed`),
`collection_changes(kind, since, limit) -> Changes<CollectionId>` (JMAP `Calendar/changes`, `AddressBook/changes`),
`account_changes(kind, since, limit) -> Changes<DavObjectId>` (JMAP `CalendarEvent/changes`, `ContactCard/changes`), `state(kind)`
(JMAP `state`). `since = 0` — всё в `created`; `has_more` — `new_state` последнего возвращённого; `since` старше очищенных надгробий или
больше счётчика — `Conflict`. Сигнал `DavChanged { kind, collection, modseq }` (`dav_changed`, учётная запись) — после фиксации каждого
изменения. Входящие планирования `inbox_put`, `inbox_list`, `inbox_delete` — раздел Schedule.
Книга GAL (CTR-60, решение 72) — одна адресная книга организации с именем `GAL_COLLECTION_NAME` = `gal` (`Collection::is_gal`; имя
зарезервировано: `create_collection` адресной книги с ним — `Conflict`), общая для CardDAV и JMAP. Появляется при первом
`sync_gal(tenant, display_name, Vec<GalCard { name, body, index }>) -> ModSeq` — служебной записи полного набора одной транзакцией
(проверка — `dav::validate_gal`: имя и индекс карточки, только `VCARD`, имена и UID без повторов — `Invalid`; `max_object_size`,
`max_objects` — `QuotaExceeded`; ничего не изменено). Карточка с прежним именем и тем же телом и индексом не меняется (ETag и `ModSeq`
прежние, в `changes` не попадает), с другим — перезапись (прежний `id`, новые ETag и `ModSeq`), с новым именем — создание, отсутствующая
в наборе — надгробие; новое отображаемое имя — изменение свойств книги; возвращается `Collection::state` книги (без изменений —
прежний). Счётчик `ModSeq` — свой у книги. Книга — среди коллекций каждой учётной записи организации (`list_collections`,
`get_collection`, `collection_by_name`; не выбирается `default_of`), читается как своя (`get_object`, `object_by_uid`, `list_objects`,
`multiget`, `in_range`, `find_cards`, `changes` по счётчику книги, `objects_by_id`); запись пользователя — `put_object`,
`delete_object`, `move_object` (из неё или в неё), `update_collection`, `delete_collection` — `Forbidden`. В `collection_changes`,
`account_changes` и `state` учётной записи книга не входит; изменения карточек по идентификаторам (JMAP `ContactCard/changes`) —
`gal_changes(tenant, since, limit) -> Changes<DavObjectId>` (нет книги — как у новой учётной записи). После записи с изменениями —
системный сигнал `GalChanged { collection, modseq }` (`dav::gal_changed`). Наборы `suite::dav::gal`, `suite::dav::gal_signals`;
PostgreSQL — META-63, потребители — DAV-29, JMAP-39, APP-49. In-memory — `MemDav` (`with_limits`,
`with_bus`, `purge_tombstones`; и `FreeBusy`); наборы `suite::dav` (лимиты `suite::dav::LIMITS`, `DavFixture`), `suite::dav::signals`.
PostgreSQL — `tp-meta` (META-46…48, планирование — META-51).

## Schedule
SRS 4.4; CTR-47. Код — `schedule.rs`, методы `inbox_*` — `dav.rs`. Планирование встреч: CalDAV Scheduling (RFC 6638), iTIP (RFC 5546),
iMIP (RFC 6047), занятость. iCalendar контракты не разбирают: разбор и построение iTIP — `tp_dav::itip` поверх calcard (DAV-13), общий
с JMAP (`sendSchedulingMessages`, JMAP-31; JMAP Calendars — по черновику IETF, решение 35). Адрес календарного пользователя —
без `mailto:`, `valid_address` (1–320 байт, без пробелов и управляющих; нормализует вызывающий).
- `ItipMethod::{Publish, Request, Reply, Add, Cancel, Refresh, Counter, DeclineCounter}` (`as_str`, `FromStr` без регистра, `from_organizer`).
- `ItipDelivery::deliver(&ItipMessage { recipient: AccountScope, to, from, method, ics, source: ItipSource::{Internal, Imip} }) ->
  ItipOutcome::{Applied, Ignored(reason)}` — приём iTIP учётной записью: `REQUEST`, `ADD` — событие в календаре по умолчанию
  (`PARTSTAT=NEEDS-ACTION`), `REPLY` — статус участника у организатора, `CANCEL` — `STATUS:CANCELLED`, все — во входящие планирования.
  `from` проверен вызывающим (iMIP — `From`, прошедший DMARC; внутри — адрес учётной записи). CTR-59 (решение 78): `Internal` — также
  приглашение из почтового клиента (submission, JMAP `EmailSubmission`; SMTP-36), если `From` письма принадлежит вошедшей учётной записи
  (свой адрес, псевдоним, право Send As); иначе — письмо без доставки iTIP. Недопустимое (`ItipMessage::validate`),
  подделанный отправитель, устаревший `SEQUENCE`, чужой `UID` организатора — `Ok(Ignored)`; `Err` — сбой хранилища (идемпотентно,
  повтор безопасен); письмо iMIP при любом итоге остаётся в ящике. Реализация — `tp-dav` (DAV-15); вызывают `tp-smtp` (SMTP-33,
  `Delivery::with_itip`), `tp-dav` (DAV-13), `tp-jmap` (JMAP-31). Заглушка — `FakeItip` (`set(to, outcome)`, `fail_next`, `calls`).
- `ItipSender::send(&ItipOutgoing { sender: AccountScope, from (ORGANIZER или ATTENDEE учётной записи), to: Vec (внешние), method, ics,
  subject, text }) -> Submitted` — письмо iMIP (`multipart/alternative`: `text/plain` и `text/calendar; method=…`) через
  `queue::Outbound::submit` (очередь и DKIM); `validate()` → `Invalid`. Реализация — `tp-dav` поверх `Outbound` (сборка — APP-28);
  in-memory — `MemItipSender` (`sent`, `fail_next`; все `Queued`).
- Входящие планирования (`schedule-inbox`, RFC 6638 §2.2) в `DavStore` (методы с реализацией по умолчанию — `Internal`):
  `inbox_put(scope, name, InboxMessage { method, uid, from, body }) -> Put` (создать или перезаписать, `id` сохраняется; UID не уникален;
  имя — `valid_name`, `validate()` → `Invalid`; тело больше `max_object_size` или сообщений больше `max_objects` — `QuotaExceeded`),
  `inbox_list(scope) -> Inbox { items: Vec<InboxItem { id, name, etag, modseq, received_at, message }> (по `ModSeq`), state }` (`state` —
  CTag входящих и JMAP `state` `CalendarEventNotification`), `inbox_delete(scope, name) -> ModSeq` (надгробие; нет — `NotFound`).
  Журнал входящих (CTR-65; обязательные методы): `inbox_state(scope) -> ModSeq` — как `Inbox::state`; `inbox_changes(scope, since, limit)
  -> Changes<DavObjectId>` (JMAP `CalendarEventNotification/changes`) — созданные, перезаписанные и удалённые (надгробия) после `since`,
  страницы и `new_state` — как у `account_changes`, устаревший `since` — `Conflict`; `InboxItem::received_at` — время первой записи (UTC,
  JMAP `created`), перезапись не меняет. Набор `suite::itip::inbox_log`; PostgreSQL — META-69 (до неё `PgMeta`: `inbox_changes` —
  `Internal`, `received_at` — из UUID v7 идентификатора, надгробий нет).
  `ModSeq` — из счётчика учётной записи; `DavStore::state` и `*changes` календарей входящие не меняют. Сигнал
  `ScheduleInboxChanged { modseq }` (`schedule_inbox_changed`, учётная запись) — после каждого `inbox_put`, `inbox_delete`.
- `Schedule-Tag` (RFC 6638 §3.2) — `ObjectIndex::schedule_tag` (как ETag, `valid_etag`): задаёт вызывающий — новый при изменении
  организатором, прежний при изменении участником только `PARTSTAT`; `If-Schedule-Tag-Match` проверяет вызывающий по прочитанному объекту
  и пишет с `Precondition::IfMatch(etag)`.
- Занятость: `ObjectIndex::busy: Vec<BusyPeriod { start, end, kind: BusyKind::{Busy, Tentative, Unavailable} }>` (`[start, end)`, UTC;
  `FBTYPE`) — периоды неповторяющегося объекта, не больше `MAX_BUSY_PERIODS` (4096), `validate_busy` → `Invalid`. Вычисляет `tp-dav`
  (DAV-16): `TRANSP:TRANSPARENT`, `STATUS:CANCELLED` — пусто, `STATUS:TENTATIVE` — `Tentative`. Повторяющееся событие (в теле `RRULE`
  или `RDATE`) — `ObjectIndex::recurring = true` и пустой `busy` (иначе `Invalid`; только `Component::Event`): его занятость хранилище
  разворачивает из тела при запросе библиотекой calcard (META-54, решение 59) — `RRULE`, `RDATE`, `EXDATE`, переопределённые вхождения,
  часовые пояса; время без пояса — в поясе календаря (`Collection::timezone`, иначе UTC); `TRANSP:TRANSPARENT` и `STATUS:CANCELLED` не
  занимают, `STATUS:TENTATIVE` — `Tentative`; горизонта нет, от начала повторов — не больше `DavLimits::max_busy_instances` вхождений
  объекта (по умолчанию 20 000), остальные в занятость не попадают; тело, которое не разбирается, занятости не даёт. `FreeBusy::query(tenant, accounts, Period { start, end }) -> Vec<(AccountId, Vec<BusyPeriod>)>` —
  календари учётной записи кроме `transparent`; запись на каждую учётную запись в порядке `accounts` (повторы — один раз), периоды,
  пересекающиеся с интервалом, обрезаны по нему и нормализованы `merge_busy` (по `(start, end, kind)`, один вид склеен, разные — нет);
  нет данных, чужая организация, несуществующая — пусто (существование и права «только занятость», CTR-48, проверяет вызывающий);
  `end ≤ start` — `Invalid`. На нём строятся VFREEBUSY (RFC 6638 §5, DAV-16), `Principal/getAvailability` (JMAP-30) и
  `Availability/get` (jmap-09). Реализации — хранилища: `MemDav`, `PgMeta` (META-51).
- Наборы: `suite::itip` (входящие, `Schedule-Tag`, занятость в индексе, `transparent`; лимиты `suite::dav::LIMITS`, `DavFixture`),
  `suite::itip::signals`, `suite::freebusy(dav, fb, fixture)`.

## Sharing
SRS 4.4, 4.5; APP-I «Политики общего доступа к календарям»; CTR-48. Код — `sharing.rs`. Права проверяет единый слой авторизации
(`tp-auth`), хранилища только хранят записи; владелец имеет все права на свои коллекции без записей.
`CollectionRights { free_busy, read, write, schedule, admin }` (`#[serde(default)]`; `NONE`, `FREE_BUSY`, `READ`, `WRITE`, `SCHEDULE`, `ALL`):
старшее включает младшие (`effective()`: `admin` — все, `write` — `read`, `read` — `free_busy`; `schedule` — отдельное), `contains`,
`intersect`, `|`. `CollectionAcl` — права на коллекции (`dav::Collection`) по субъектам, как у `AclStore` (имя входа, группа, `anyone`;
без отрицательных прав): `collection_acl(owner: AccountScope, collection) -> Vec<CollectionAclEntry { identifier, rights }>` (нет
коллекции — `NotFound`), `set_collection_acl(owner, collection, identifier, rights)` — замена целиком, пустые права удаляют запись,
`collection_rights(owner, collection, identifiers) -> CollectionRights` — объединение записей субъекта (`sharing::collection_rights`),
`shared_with(scope, identifiers) -> Vec<SharedCollection { owner, collection, rights }>` — чужие коллекции для дома коллекций получателя,
по (`owner`, `collection`). Удаление коллекции или учётной записи удаляет записи.
Представители: `Delegate { delegate: AccountId, level: DelegateLevel::{Read, Write}, schedule_on_behalf }` — доступ ко всем календарям
владельца (CalDAV `calendar-proxy-read`/`-write`) и планирование от его имени (`SENT-BY`); `Delegate::rights()` — `READ` или `WRITE`
плюс `schedule`. `DelegateStore`: `delegates(owner)`, `set_delegate(owner, Delegate)` (сам себе — `Invalid`), `remove_delegate(owner,
delegate)`, `delegations(delegate) -> Vec<Delegation { owner, level, schedule_on_behalf }>` (`calendar-proxy-*-for`).
Ресурсы (переговорные) — учётные записи вида `meta::AccountKind::Resource`: вид — поле `Account::kind`, задаётся `AccountUpsert::kind`,
тип (помещение или оборудование, CTR-88) — `Account::resource_type` (раздел Metadata), отдельного метода нет. `ResourceStore` — правила бронирования: `booking_policy(scope) -> Option<BookingPolicy>`
(`None` — не заданы или не ресурс), `set_booking_policy(scope, Option<_>)` (учётная запись не ресурс — `Invalid`; вид правила не
меняют), `resources(tenant)` — учётные записи вида `Resource`.
`BookingPolicy { auto_accept, allow_conflicts, max_duration: Option<_>, horizon: Option<_>, bookers: Bookers::{Internal, Only { accounts,
groups }}, working_hours: Option<WorkingHours { timezone (IANA), days: BTreeSet<DayOfWeek>, from, to }> }` (`#[serde(default)]`,
длительности — humantime; по умолчанию — автоматическое принятие без пересечений, до 24 ч, на 180 дней вперёд, круглосуточно, все учётные
записи организации; `validate()` → `Invalid`). `BookingPolicy::decide(&BookingRequest { organizer: Option<AccountId>, organizer_groups,
periods: Vec<BookingPeriod { period, local_start }>, unbounded }, busy: &[BusyPeriod], now) -> BookingDecision::{Accept,
Decline(DeclineReason), Tentative}` — чистая функция; проверки по порядку: организатор (внешний — никогда; `NotAllowed`), вхождения
(`Invalid`), длительность (`TooLong`), горизонт (`TooFar`; повторы без конца при заданном горизонте), рабочее время
(`OutsideWorkingHours`; местное время вхождения вычисляет вызывающий), пересечения (`Conflict`); прошло всё — `Accept` при `auto_accept`,
иначе `Tentative`. Повторы разворачивает и занятость (`FreeBusy`) читает вызывающий (`tp-dav`, обработка iTIP).
Политика организации: `SharingPolicy { scope: SharingScope::{Nobody, Groups { groups }, Organization}, max_outside: CollectionRights }`
(по умолчанию — `Organization`, `max_outside = READ`): `ceiling(anyone, in_group)` — наибольшие права субъекту (вне групп и субъекту
`anyone` — не больше `max_outside`), `check(rights, anyone, in_group)` — при выдаче (`Forbidden`; отзыв допустим всегда), `limit(rights, …)`
— при вычислении действующих прав (ужесточение политики действует на выданные записи). `SharingPolicyStore`: `sharing_policy(tenant)`,
`set_sharing_policy(tenant, policy)`. Реализации — `tp-meta`, in-memory и наборы — `tp-testkit`; проверка прав — `tp-auth`; CalDAV, CardDAV,
JMAP (`shareWith`, `myRights`) — `tp-dav`, `tp-jmap`; Admin API — `tp-admin`.
Проверка прав (CTR-84): `SharingAuthorizer` — единый слой прав на коллекции для протоколов, отдельно от `Authorizer` (папки и отправка);
реализация — `tp_auth::CollectionAuthorizer` поверх `CollectionAcl`, `DelegateStore`, `SharingPolicyStore`, `DavStore`, `GroupStore`,
`AccountStore`; узел — `Services::collections`. Методы обязательные: `rights(&Principal, owner: AccountId, collection) -> CollectionRights`
(владелец — `ALL` без записей, прочие — записи по имени входа, группам и `anyone` и представительство на календари, ограниченные
`SharingPolicy::limit`; нет коллекции — `NONE`), `check(.., required)` (`Forbidden`), `shared_with(&Principal) -> Vec<SharedCollection>`
(чужие коллекции с действующими правами, по (`owner`, `collection`); снятые политикой не входят), `delegations(&Principal) ->
Vec<Delegation>` (уровень в пределах политики, по `owner`), `check_grant(grantor, owner, collection, identifier, rights)` (владелец или
`admin`, в пределах `SharingPolicy::check`, отзыв — всегда; недопустимый идентификатор — `Invalid`, иначе `Forbidden`),
`check_delegate(&Principal, owner, &Delegate)` (только владелец, в пределах политики). Набор — `suite::sharing_authorizer`
(`SharingAuthorizerFixture` — хранилища реализации, организация набора — новая); фейк для протоколов — `FakeSharingAuthorizer` (права по
таблице, без политики).
Уведомления о доступе (CTR-86; JMAP `ShareNotification`, RFC 9670 §5): `ShareNotification { id: ShareNotificationId (задаёт вызывающий),
created, changed_by: ShareChangedBy { name, email?, account? }, object_type: SharedObjectType::{Calendar, TaskList, AddressBook}, owner,
collection, old_rights, new_rights (`NONE` — не было или отозваны), name }` (`validate()`: права не изменились — `Invalid`); уведомления
не меняются. `ShareNotificationStore` — общее для узлов, видит только получатель: `record(recipient, n) -> u64` (занятый id — `Conflict`;
сверх `MAX_NOTICES` = 1000 у получателя вытесняет старшие по (`created`, `id`) — как удаление, в `destroyed` изменений; CTR-90),
`notifications(scope)` — по (`created`, `id`), `notification(scope, id) -> Option`, `destroy_notification(scope, id) -> u64` (`/set destroy`;
нет — `NotFound`), `notification_state(scope) -> u64` (у новой — 0), `notification_changes(scope, since, limit) ->
ShareNotificationChanges { created, destroyed, new_state, has_more }` (записанное и удалённое после `since` не входит; `limit` = 0 —
`Invalid`; состояние из будущего или старше истории — `Conflict`), `expire_notifications(before, limit) -> usize` — срок хранения во всех
организациях (удалённые — в изменениях; срок — настройка протокола). Фильтр `/query` — `ShareNotificationFilter { after, before,
object_type, owner }::matches`. Записывает вызывающий, изменивший права (`CollectionAcl::set_collection_acl`), — каждой учётной записи с
изменившимися правами; удаление учётной записи удаляет её уведомления. Все методы обязательные; in-memory — `MemShareNotifications`; набор
`suite::sharing::share_notifications`; PostgreSQL — `PgMeta` (META-83, миграция 0042).

## Branding
Решение 122; CTR-91. Код — `branding.rs`. Брендинг заказчика вместо IT TASK в веб-консоли, текстовом интерфейсе и подсветке CLI.
`BrandColor` — цвет `#RRGGBB` (`FromStr` в любом регистре, `Display` и serde строкой — верхний регистр, иное — `Invalid`; `from_rgb`,
`from_hex`, `rgb`, `luminance`/`contrast` — WCAG 2.x, `text_on` — белый или чёрный с большим контрастом). Роли — `design/tokens.toml`:
`Role` (`[roles]`: screen, panel, text, muted, frame, frameA, titleA, head, sel, selFg, selI, selIFg, fkBg, fkFg, ok, warn, err; по
темам `Theme::{Light, Dark}`) и `CliRole` (`[cli.roles]`: head, accent, ok, warn, err, muted, json_key, json_number, json_bool); имена в
JSON — как в файле. Цвета IT TASK — `Role::it_task(theme)`, `CliRole::it_task()`, `Palette::it_task()` (розовый выделения — `#E3107F`,
решение 122; тест сверяет роли, цвета и пары с файлом токенов). `CONTRAST` — пары `[[contrast]]` (`ContrastPair { fg, bg, min }`: текст —
4,5, рамки и заливки — 3), роли CLI — `CLI_MIN_CONTRAST` (3) на обоих `CLI_BACKGROUNDS`; исключений нет.
`Palette { light, dark, cli }` — карты ролей (`role(theme, r)`, `cli_role(r)`: нет роли — цвет IT TASK); `contrast_issues() ->
Vec<ContrastIssue { theme (None — CLI), fg, bg, fg_color, bg_color, contrast, min }>` — пары ниже порога.
`BrandLogo { hash: BlobHash, size, width, height }` — знак PNG в BLOB по пути `path()` = `branding/<hash>.png` (`BlobStore::put_raw`:
путь по содержимому неизменяем); `validate()` — 1 байт–`MAX_LOGO_SIZE` (256 КБ), стороны 1–`MAX_LOGO_SIDE` (1024); формат по
содержимому проверяет вызывающий. `Branding { product_name?, primary?, light, dark, cli, logo? }` — набор уровня, обязательных полей нет
(`Default` — пустой); `validate()` — название 1–`MAX_PRODUCT_NAME` (64) символов без управляющих, знак; `has_colors()`;
`palette_over(parent) -> Palette` — чистая функция вывода: заданная роль — как задана; при основном цвете уровня роли бренда `frameA`,
`fkBg`, `sel`, `head` (и CLI `head`, `json_key`) — из него, `titleA` — из `sel`, `selFg`/`fkFg` — `text_on` к `sel`/`fkBg` (также если
уровень задал `sel`/`fkBg`), CLI `accent`, `json_bool` — из `sel` светлой темы; выведенные роли подгоняются смесью с чёрным (светлая
тема) или белым (тёмная) с шагом 1 % до порогов своих пар; остальные роли — с `parent` как есть. `BrandingScope::{Installation,
Org(TenantId)}`. `BrandingLevels { installation?, org? }::effective() -> EffectiveBranding { product_name?, logo?, palette }` — каждое
поле с ближайшего уровня (организация → установка → встроенное); цвета — установка поверх IT TASK, затем организация поверх итога; уровень
без цветов цвета не меняет; со страховкой отрисовки `Palette::guarded(parent)` (`Branding::safe_palette_over`): пара ниже порога берёт обе
роли с уровня выше. Проверка при сохранении (без страховки) — `BrandingLevels::contrast_issues()` (организация поверх действующих цветов
установки) и `installation_change_issues(installation?, &[(TenantId, Branding)]) -> Vec<OrgContrastIssue { org?, issue }>` (сам набор
установки — `org: None`, и действующая палитра каждой организации поверх него). `BrandingStore`: `branding(scope) -> Option` — свой набор уровня; `set_branding(scope, &Branding)` — замена
целиком (`validate()`; неверный — `Invalid`, прежний остаётся); `reset_branding(scope) -> Option` — удаляет и возвращает прежний (знак из
BLOB удаляет вызывающий, если `logo_in_use` ложно); `branding_levels(Option<TenantId>) -> BrandingLevels` — оба уровня одним чтением
(`None` — только установка); `org_brandings() -> Vec<(TenantId, Branding)>` — свои наборы всех организаций по возрастанию; `logo_in_use(hash)`. Права уровней, журнал аудита и отказ по контрасту — Admin API (ADM-48). Все методы обязательные; in-memory — `MemBranding`; набор
`suite::branding`; PostgreSQL — `PgMeta` (META-84, миграция 0043).

## Sieve
SRS 4.3; CTR-46. Код — `sieve.rs`. Интерпретатор — свой крейт `tp-sieve` (sieve-rs под AGPL-3.0); ManageSieve — в роли `imap` (как POP3),
секция `sieve` крейта `tp-sieve`; `Role` не меняется. `SieveStore` (по `AccountScope`; текст проверяет вызывающий): `limits() ->
SieveLimits { max_scripts, max_script_size }` (из конфигурации реализации; превышение — `QuotaExceeded`), `list -> Vec<ScriptInfo { name,
size, active }>` (по имени), `get(name)`, `active() -> Option<ActiveScript { name, text }>`, `put(name, text)` (имя — `valid_script_name`,
RFC 5804; замена сохраняет активность), `rename(from, to)` (занято — `Conflict`, активность переходит), `delete` (активный — `Conflict`),
`activate(Option<name>)` (активный не больше одного; `None` — снять), `org_script(tenant)` / `set_org_script(tenant, Option<text>)` (скрипт
организации, выполняется до скрипта пользователя; без сигнала), `vacation()` / `set_vacation(Vacation { enabled, from, to, subject,
text_body, html_body })` (JMAP `VacationResponse`, RFC 8621 §8; `validate()`, `is_active(now)`: `from ≤ now < to`), `state()` — растёт при
каждом изменении скриптов и автоответа (JMAP `state`, версия для кэша программ). Изменяющие методы возвращают новое состояние.
`VacationLog::seen(scope, sender, handle, period) -> bool` — атомарно «уже отвечали за период» или отметка (RFC 5230 `:days`).
Точка правил доставки `DeliveryRules::evaluate(&RulesInput { scope, recipient (с подадресом), mail_from, message: Bytes, mailboxes })
-> RulesOutcome { keep: Vec<KeepTo { mailbox: MailboxRef::{Inbox, Id, Name { name, create }}, flags }>, redirect, discard,
vacation: Option<AutoReply { to, from, subject, body: ReplyBody::{Text, Html, Mime}, in_reply_to, references }>, errors }`;
порядок — скрипт организации, автоответ, активный скрипт; ошибка скрипта — implicit keep (`RulesOutcome::failed`) и `errors` для журнала;
`Err` — сбой хранилища: вызывающий доставляет во «Входящие». Нет правил — `RulesOutcome::implicit_keep()`. Письмо автоответа собирает
`tp-smtp` (RFC 3834). Сигнал `SieveChanged` (`sieve_changed`, учётная запись) — после изменения скриптов и автоответа. In-memory — `MemSieve`
(`with_limits`, `with_bus`), `MemVacationLog`, фейк `FakeRules` (итог по получателю, `fail_next`, `calls`); наборы `suite::sieve`
(лимиты `suite::sieve::LIMITS`, `SieveFixture`), `suite::sieve::{vacation_log, signals}`. PostgreSQL — `tp-meta` (META-49, META-50).

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
- 2026-11-11 — CTR-35 (x/contracts-23, решение 2026-11-04-exchange-parity): `audit::{MailboxAudit, AuditEntry, AuditPolicy, AuditQuery}` — аудит
  доступа к ящику с политикой действий владельца; `senders::{SenderLists, SenderList, SenderVerdict}` — личные списки отправителей;
  `addressbook::{ContactStore, OrgStore, DynamicGroupStore, MemberFilter}` — почтовые контакты, оргструктура, динамические группы; ids
  `ContactId`, `OrgUnitId`; `Directory`: `DirAccount::org`, `contacts_since`, `units`, `SyncGate` и порог массового удаления; `AccountStore`:
  `AccountState`, отключение, восстановление, подключение к другой учётной записи, окончательное удаление; `QueueStore`: срок выпуска
  (`next_attempt`, `queued_at`), `cancel_queued`; `DeliveryLog::by_queue`. Новые методы существующих трейтов — по умолчанию (`Internal`,
  `account_state` — `Active`, `Directory` — пусто). In-memory — `MemMailboxAudit`, `MemSenderLists`, `MemAddressBook`, `MemSyncGate`,
  `MemMetadata`, `MemQueue`, `MemDeliveryLog`, `FakeDirectory`; наборы `suite::{mailbox_audit, sender_lists, contact_store, org_store,
  dynamic_groups, sync_gate, account_state, queue_release, delivery_by_queue}`. Реализации — META-33, AUTH-10, ADM-10, SMTP-24…26, JMAP-15…18,
  IMAP-15, MS-16/17.
- 2026-11-11 — CTR-42 (x/contracts-30, решение владельца #195): отложенная отправка и отмена — `Outbound::{schedule, cancel_scheduled}`
  (очередь со сроком выпуска, отмена до выпуска), `SubmissionStore::{query_submissions, submission_changes, expire_submissions}`,
  `SubmissionFilter`, `SubmissionChanges`; описание потока в `submission.rs` (воркер `due` только фиксирует наступившие). In-memory —
  `MemOutbound` (`Scheduled`, `release`), `MemSubmissions` (журнал изменений); наборы `suite::{submission_query, outbound_schedule}`.
- 2026-11-12 — x/jmap-08: `SubmissionStore::{submission_changes, expire_submissions}` и `DeliveryLog::by_queue` — обязательные (тела
  по умолчанию с `Internal` удалены); реализации — `tp-meta` (PostgreSQL), `MemSubmissions`, `MemDeliveryLog`.
- 2026-11-11 — CTR-43 (x/contracts-31, решение 2026-11-11-stage2-followups п. 9): очередь модерации и для правил потока почты —
  `HeldMessage::{group: Option<_>, source: HoldSource, moderators: Vec<Moderator>, recipients}`, `HeldMessage::validate`,
  `Moderation::deliver(.., Option<&DistributionGroup>, ..)` (без группы — исходным получателям), `check_delivery` для обоих случаев. In-memory —
  `MemGroups`, `MemModeration` (`Deliver(_, Option<GroupId>)`); набор `suite::rule_moderation`. `tp-admin` — источник в очереди модератора,
  одобрение письма правила; `tp-meta` — чтение и запись писем групп (письмо правила — `Internal` до META-44); `tp-smtp`, `tp-auth`, `thepost` —
  новые поля и сигнатура.
- 2026-11-11 — CTR-45 (x/contracts-33): `dav::{DavStore, Collection, CollectionKind, Component, DavObject, ObjectIndex, Span, TimeRange,
  Precondition, PutOutcome, DeleteOutcome, DavLimits, dav_changed}`, `CollectionId`, `DavObjectId`, `EventKind::DavChanged` — календари и
  контакты Ф2. По сравнению с карточкой: методы с префиксами (`*_collection`, `*_object`), `collection_by_name`, `objects_by_id`,
  `collection_changes`, `state(kind)` для JMAP; итог записи — `PutOutcome` (412 и `no-uid-conflict` с именем ресурса различимы, не `Conflict`);
  UID — в `ObjectIndex`. In-memory — `MemDav`; наборы `suite::dav`, `suite::dav::signals`. Реализация в `tp-meta` — META-46…48.
- 2026-11-11 — CTR-46 (x/contracts-33): `sieve::{SieveStore, ScriptInfo, ActiveScript, SieveLimits, Vacation, VacationLog, DeliveryRules,
  RulesInput, RulesOutcome, KeepTo, MailboxRef, AutoReply, ReplyBody, sieve_changed}`, `EventKind::SieveChanged`. По сравнению с карточкой:
  `SieveStore::active`, `limits`, `state`; `MailboxRef::Inbox` для implicit keep; `AutoReply::references`. In-memory — `MemSieve`,
  `MemVacationLog`, `FakeRules`; наборы `suite::sieve`, `suite::sieve::{vacation_log, signals}`. Реализация в `tp-meta` — META-49, META-50.
- 2026-11-12 — CTR-47 (x/contracts-34): `schedule::{ItipDelivery, ItipMessage, ItipMethod, ItipSource, ItipOutcome, ItipSender,
  ItipOutgoing, InboxMessage, InboxItem, Inbox, FreeBusy, Period, BusyPeriod, BusyKind, merge_busy, validate_busy, valid_address,
  schedule_inbox_changed}`, `DavStore::{inbox_put, inbox_list, inbox_delete}` (реализация по умолчанию — `Internal`),
  `ObjectIndex::{busy, schedule_tag}` и `ObjectIndex::new`, `Collection::transparent`, `DavLimits::busy_horizon`,
  `EventKind::ScheduleInboxChanged`. По сравнению с карточкой: в `ItipMessage` нет отдельного `tenant` (он в `recipient`), добавлены
  `to` (адрес получателя) и `source`; `ItipOutgoing` с темой и текстом письма; интервал запроса — `Period`; результат `FreeBusy::query`
  нормализован `merge_busy`; `Collection::transparent` (RFC 6638 `schedule-calendar-transp`) — для исключения из занятости; `Inbox::state`
  и сигнал входящих — для CTag и JMAP `CalendarEventNotification`; горизонт занятости — в `DavLimits`. Новые поля индекса и коллекции —
  `#[serde(default)]`. In-memory — `MemDav` (входящие, `FreeBusy`), `FakeItip`, `MemItipSender`; наборы `suite::itip`,
  `suite::itip::signals`, `suite::freebusy`. Реализация в `tp-meta` — META-51, в `tp-dav` — DAV-13, DAV-15, DAV-16.
- 2026-11-12 — META-54 (x/meta-35), правка CTR-47 по решению 59: занятость повторяющихся событий разворачивается при запросе.
  Добавлено `ObjectIndex::recurring` (`#[serde(default)]`; задаёт вызывающий — `tp-dav`, `tp-jmap`), `DavLimits::max_busy_instances`;
  **удалено** `DavLimits::busy_horizon` и ключ конфигурации `meta.dav.busy_horizon` (секция `deny_unknown_fields`: ключ из конфигурации
  убрать, вместо него — `meta.dav.max_busy_instances`). `ObjectIndex::busy` — только у неповторяющихся объектов. Разворот — в хранилищах
  (`PgMeta`, `MemDav`) библиотекой calcard; набор `suite::freebusy_recurring`. Миграция `tp-meta` 0025.
- 2026-11-12 — CTR-49 (x/contracts-36): `QueueStore::enqueue(owner: Option<AccountId>, msg)`, `BlobIndex::acquire(tenant, owner:
  Option<AccountId>, hash, refs)` — учётная запись отправителя или владельца для выбора шарда (решение 46); `EventKind::ShardsChanged` —
  системный сигнал изменения справочника шардов (решение 47). Вызовы в крейтах — `None` (учётная запись на этих путях неизвестна).
  `tp-meta`: `enqueue` со справочником — на шард `owner`; `acquire` — по-прежнему основная БД (шард учёта BLOB требует учётной записи
  и в `commit_new`/`release`); `ShardDirectory::follow` сбрасывает кэш по `ShardsChanged`. In-memory `owner` не используют; набор
  `suite::signals` — с `ShardsChanged`.
- 2026-11-12 — CTR-50 (x/contracts-36): `meta::ExistingContent::{Keep, Delete}` и `AccountStore::attach_account(.., existing:
  Option<ExistingContent>)` — содержимое ящика учётной записи при подключении отключённого (решение 44): перенос в подключаемый ящик
  (папки с суффиксом при совпадении имён) или в его корзину; без режима и с письмами — `Invalid` с подсказкой (было `Conflict`).
  По сравнению с карточкой: перенос — часть операции хранилища, фоновую задачу ведёт Admin API (ADM-10). In-memory — `MemMetadata`;
  набор `suite::account_state`. PostgreSQL — META-33.
- 2026-11-12 — CTR-51 (волна contracts-37): `acquire::{AcquireObserver, AcquireWait, AcquireEnd, NoAcquireObserver, AcquireCounter}` —
  наблюдатель ожидания соединения с СУБД (число ожидающих, время, отказы; отмена снимает «ожидает» без записи времени). По сравнению
  с карточкой: счётчик в памяти `AcquireCounter` — в `tp-contracts`, а не в `tp-testkit`.
- 2026-11-12 — CTR-52 (волна contracts-37): `InternalTls.key: Option<SecretRef>` — ключ внутренних каналов из хранилища секретов
  (взамен `key_file`, оба сразу — `Invalid`), `InternalTls::key_source()` и `KeySource::{File, Secret}`. Новое поле секции: код,
  собирающий `InternalTls` перечислением всех полей, дополняется `key: None` (в репозитории таких мест нет).
- 2026-11-12 — CTR-25, CTR-67, CTR-70 (волна contracts-46, аудит 12.11, решения 93, 97, 98, 104, 105): `meta::{PolicyTarget, PolicyStore}`
  — уровень цепочки политик и запись уровня (один `PolicyTarget` на систему; `tp_testkit::PolicyLevel` удалён, `MemPolicies` реализует
  `PolicyStore`, синхронные `MemPolicies::policy`/`set_policy` удалены); `admin_audit::{AuditLog, AdminAuditRecord, AdminAuditEntry,
  AdminAuditQuery, AdminAuditPage}` — журнал аудита Admin API, `MemAuditLog`, наборы `suite::{policy_store, admin_audit_log}`.
  `Policy`: потребители полей и умолчания уровня организации полей SMTP (`DEFAULT_SEND_PER_HOUR`, `DEFAULT_MAX_RECIPIENTS`,
  `send_per_hour_or_default`, `max_recipients_or_default`), исполнитель `retention` — модуль «Соответствие требованиям».
  `TenantTransport.rewrites` — обязательное поле (без `serde(default)`). Временные трейты `tp-admin` (`PolicyStore`, `PolicyTarget`,
  `AuditLog`) и `PolicyTarget` `tp-meta` заменяются в META-70 и ADM-34.
- 2026-11-12 — CTR-79 (волна contracts-51, решение 113): `Policy::rcpts_per_hour` — лимит отправки «адресатов в час» на учётную запись
  по цепочке организация → домен → ящик, `Policy::DEFAULT_RCPTS_PER_HOUR` = 2000, `rcpts_per_hour_or_default()`; потребитель — SMTP
  (SMTP-43). Хранение — столбец `policies.rcpts_per_hour` (миграция 0026), Admin API — поле `rcpts_per_hour` политики уровня. Код,
  собирающий `Policy` перечислением всех полей, дополняется `rcpts_per_hour`.
- 2026-11-12 — CTR-58 (волна contracts-40, решение 79): `DirAccount::hidden_from_gal` (`#[serde(default)]`) — учётная запись скрыта
  из адресной книги без запрета входа. Код, собирающий `DirAccount` перечислением всех полей, дополняется `hidden_from_gal` (в репозитории —
  `tp-auth` и тесты `tp-testkit`, `tp-dav`, `tp-smtp`: `false`); чтение признака из LDAP — AUTH-16, фильтр GAL — DAV-27.
- 2026-11-12 — MS-19 (волна mailstore-12, класс B): `TrashStore::hidden_message(scope, id) -> Option<Message>` — метаданные скрытого
  письма для администратора без снятия скрытия (без реализации по умолчанию; `MemTrash`, `PgMeta`); набор `suite::trash_purge` проверяет
  его. Потребитель — Admin API: тема и отправитель скрытых писем на странице `trash/hidden` (из `Message::meta.envelope`, без чтения BLOB).
- 2026-11-12 — CTR-59 (волна contracts-42, решения 59, 78): занятость повторяющихся событий — разворот при запросе — уже в контракте
  по META-54 (`ObjectIndex::recurring`, `DavLimits::max_busy_instances`, удалён `DavLimits::busy_horizon`; набор
  `suite::freebusy_recurring`: встреча без конца занята через 3 года, `EXDATE` свободна). Добавлено: `ItipSource::Internal` — и приглашение
  из почтового клиента с `From` вошедшей учётной записи (SMTP-36). Код не меняется — только описание.
- 2026-11-12 — CTR-53 (волна contracts-38): `Account::aliases`, `AccountUpsert::aliases` и `Account::addresses()` — адреса учётной
  записи (основной и псевдонимы каталога без catch-all); правила — `meta::account_addresses`, `meta::account_aliases`. Несовместимо:
  новые обязательные поля `Account` и `AccountUpsert`. `tp-meta` — миграция 0030 (`accounts.aliases`).
- 2026-11-12 — CTR-55 (волна contracts-38): `GroupStore::held(tenant, group, after, limit)` — очередь модерации страницами по курсору
  `HeldCursor { held_at, id }`; правило страницы — `groups::held_page`. Несовместимо: новый параметр `after` у `held`.
- 2026-11-12 — CTR-54 (волна contracts-38): `DavStore::move_object` и `MoveOutcome`/`Moved` — перенос объекта между коллекциями с
  сохранением `id`; поиск контактов — `ObjectIndex::search: CardSearch` (пишется `put_object`) и `DavStore::find_cards`. Несовместимо:
  оба метода обязательны для реализаций, новое поле `ObjectIndex`. `tp-meta` — миграция 0030 (`dav_objects.search`, `placed_modseq`).
- 2026-11-12 — CTR-56 (волна contracts-38): поля `Collection` — `is_default`, `subscribed`, `visible`, `alerts_with_time`,
  `alerts_without_time`; признак по умолчанию переносится записью коллекции (`create_collection`, `update_collection`), правило
  выбора — `dav::default_of`. Несовместимо: новые поля `Collection`. `tp-meta` — миграция 0030 (столбцы `dav_collections`).
- 2026-11-12 — CTR-57 (волна contracts-39): адрес клиента в проверке учётных данных — параметр `peer` у `Authenticator::verify` и поле
  `SaslContext::peer`. Несовместимо: новая сигнатура `verify`, новое поле `SaslContext`; все реализации и вызовы (`tp-auth`, `tp-imap`,
  `tp-smtp`, `tp-sieve`, `tp-jmap`, `tp-dav`, `tp-admin`, `tp-testkit`, `thepost`) переведены в том же PR; сервер HTTP
  `tp_platform::service_http` передаёт обработчикам адрес соединения (`ConnectInfo<SocketAddr>`).
- 2026-11-12 — CTR-48 (волна contracts-35): раздел Sharing, модуль `sharing` — `CollectionRights`, `CollectionAcl` (права на коллекции,
  `shared_with`), представители (`Delegate`, `DelegateStore`), ресурсы (`AccountKind`, `BookingPolicy::decide`, `ResourceStore`), политика
  организации (`SharingPolicy`, `SharingPolicyStore`). Вид учётной записи — поле `Account::kind` и `AccountUpsert::kind`
  (`meta::AccountKind`), хранится с учётной записью (`tp-meta` — миграция 0031 `accounts.kind`, `tp-testkit`); несовместимо: новые
  обязательные поля `Account` и `AccountUpsert`. Реализации новых хранилищ в памяти и наборы — отдельной волной `tp-testkit`.
- 2026-11-12 — CTR-63, CTR-64 (волна contracts-44): `AccountStore::by_ids(tenant, &[AccountId])` — учётные записи по списку (порядок
  запроса, не найденные пропускаются, повтор — одна запись; по умолчанию — `get` по одному, PostgreSQL одним запросом — META-66),
  набор `suite::account_by_ids`; `EventKind::AccountDisabled { account }` — системный сигнал запрета входа и `meta::account_disabled`,
  набор `suite::signals`. По сравнению с карточками: повторы в `by_ids` схлопываются; сигнал — и при `disable_account` (вход запрещён
  и там). Публикация в `tp-meta` — META-67, подписчики — IMAP-22, JMAP-41.
- 2026-11-12 — x/admin-parity (правило владельца 12.11 «старое рядом с новым не оставлять»): у `AccountStore::restore_account`,
  `disabled_accounts`, `attach_account` нет тел по умолчанию (были `Internal`) — реализации `PgMeta`, `MemMetadata`. `SyncGate` на
  PostgreSQL — `PgMeta` (миграция 0032 `sync_holds`), набор `suite::sync_gate`. `tp_admin::Deps` — хранилища CTR-35 обязательны
  (`contacts`, `org`, `dynamic`, `senders`, `mailbox_audit`, `sync`) вместо `Admin::with_parity`.
- 2026-11-13 — CTR-60 (волна contracts-41, решение 72): книга GAL организации в `DavStore` — `sync_gal(tenant, display_name, cards)`
  (полный набор `GalCard` одной транзакцией; неизменная карточка сохраняет ETag и `ModSeq`, исчезнувшая — надгробие),
  `gal_changes(tenant, since, limit)`, `GAL_COLLECTION_NAME`, `Collection::is_gal`, `validate_gal`; книга видна каждой учётной записи
  организации, запись пользователя — `Forbidden`, имя `gal` у адресной книги зарезервировано (`Conflict`), `default_of` её не выбирает;
  сигнал `EventKind::GalChanged { collection, modseq }` (`dav::gal_changed`). Несовместимо: новые обязательные методы `DavStore`
  (без реализации по умолчанию) — `MemDav` реализует, `PgMeta` до META-63 возвращает `Internal`. По сравнению с карточкой: параметр
  `display_name` у `sync_gal`, метод `gal_changes` и сигнал `GalChanged` — для JMAP-39 (изменения по идентификаторам при своём счётчике
  книги) и push. Наборы `suite::dav::gal`, `suite::dav::gal_signals`.
- 2026-11-12 — CTR-81 (волна contracts-53, решение 114): `AuditAction::Search` — поиск по тексту писем в чужом ящике; строка запроса —
  в `AuditObject::detail`, без найденных писем; в `AuditAction::OWNER` не входит (`AuditPolicy::records` для владельца — только действия
  из `OWNER`). Несовместимо: новый вариант `AuditAction`; исчерпывающих `match` по нему в коде нет. `tp-meta` — миграция 0034
  (`mailbox_audit_action_check`), `tp-admin` — описание параметра `action`; набор `suite::mailbox_audit` — запись `Search`. Запись
  протоколами — IMAP-23, JMAP-43.
- 2026-11-13 — CTR-62, CTR-65, CTR-66 и обязательный `by_ids` (волна contracts-45). `AccountStore::by_address(tenant, address) ->
  Option<AddressTarget::{Account, Group}>` — основной адрес и псевдоним — учётная запись, адрес группы — группа; `MemMetadata::with_groups`,
  набор `suite::account_by_address`; `PgMeta` до META-68 — без псевдонимов. `AccountStore::by_ids` — без реализации по умолчанию
  (`MemMetadata`, обёртка `Limited` в тестах `tp-imap`; `PgMeta` — META-66). `DavStore::inbox_state`, `inbox_changes` (надгробия
  входящих), `InboxItem::received_at`; набор `suite::itip::inbox_log`; `PgMeta` — META-69 (до неё `inbox_changes` — `Internal`).
  `DeliveryEventKind::Itip` и `delivery::itip_record`; `tp-meta` — миграция 0035 (`delivery_log.kind`). Несовместимо: новые
  обязательные методы `AccountStore`, `DavStore`, поле `InboxItem::received_at`, вариант `DeliveryEventKind`. По сравнению с карточками:
  методы без реализаций по умолчанию (правило владельца); итог iTIP — в `decision` и `reason`, а не полями варианта (событие — строка в
  хранилище и Admin API).
- 2026-11-13 — CTR-72 (волна contracts-48): `AccountStore::by_external_id` — без реализации по умолчанию (было `Internal`: реализация
  могла «забыть» метод). Реализации — `PgMeta`, `MemMetadata` (уже были), обёртка `Limited` в тестах `tp-imap`. Несовместимо: новый
  обязательный метод `AccountStore`.
- 2026-11-13 — CTR-73, CTR-74, CTR-76, CTR-77, CTR-78 (волна contracts-50). Очередь без опроса: `QueueStore::next_due()` и сигнал
  `QueueReady` (`queue::queue_ready`, тема `<tenant>/queue/outbound`) после `enqueue`, `reschedule`, `postpone`; `MemQueue::with_bus`.
  `Outbound::enqueue`, `submit`, `schedule` получают `owner: Option<AccountId>` (решение 46), `schedule` обязателен; места вызова
  (`tp-smtp`, `tp-jmap`, `tp-dav`) переведены, `tp_smtp::queue::Queue` передаёт учётную запись в `BlobIndex::acquire` и
  `QueueStore::enqueue`. Архив: `Policy::archive_after`, `archive_quota_bytes`, модуль `archive` (`ArchiveStore::archive_due`,
  `archive_usage`), основная квота — без ветки архива; `MemArchive`. Отключённые ящики: `EventKind::DisabledMailboxChanged` и
  `meta::disabled_mailbox_changed`, `AccountState::Removing`, `AccountStore::begin_removal`; `remove_disabled` — только удаляемого;
  `disable_account`, `expired_accounts`, `remove_disabled` — без реализаций по умолчанию; `MemMetadata::with_bus`. `tp-meta` —
  миграция 0036 (`policies.archive_after_us`, `archive_quota_bytes`, `accounts.removing`). Наборы `suite::lifecycle::{queue_next_due,
  archive, disabled_mailbox_signals, account_removal}`. Несовместимо: новые обязательные методы `QueueStore`, `AccountStore`, новая
  сигнатура `Outbound`, новые поля `Policy`, варианты `AccountState` и `EventKind`. По сравнению с карточками: у `next_due` нет
  параметра вида — очередь одна (исходящая почта), вид темы — `outbound`; `owner` получает и `submit` (по умолчанию вызывает
  `enqueue`); `begin_removal` принимает `now` (проверка срока на часах вызывающего).
- 2026-11-13 — CTR-83 (волна contracts-55): `Account::hidden_from_gal`, `AccountUpsert::hidden_from_gal` — учётная запись скрыта из
  книги «Организация» без LDAP (раньше скрыть можно было только группу). Без значения по умолчанию: все места записи задают признак,
  обновление через Admin API (`PATCH` имени и блокировки) и `init` сохраняют прежний, синхронизация каталога переносит
  `DirAccount::hidden_from_gal`. `tp-meta` — миграция 0038 (`accounts.hidden_from_gal`), `impl Directory for PgMeta` отдаёт признак в
  `DirAccount::hidden_from_gal`; `MemMetadata`. Наборы: `suite::account_external_id` — признак хранится и заменяется записью;
  `pg_directory` — признак в каталоге, новая версия выгрузки. Несовместимо: новое обязательное поле `Account`, `AccountUpsert`.
  По сравнению с карточкой: реализации в той же волне (правило владельца «старое рядом с новым не оставлять»); скрытая учётная запись
  остаётся в `accounts_since` с признаком, а не пропадает из выгрузки — как у LDAP (AUTH-16): из книги её убирает построение GAL
  (DAV-27), ссылки `DirOrg::manager` на неё остаются. `PATCH` признака в Admin API и консоль — отдельные волны.
- 2026-11-14 — CTR-88 (волна contracts-59, решение 121): `meta::ResourceType::{Room, Equipment}` и поля `Account::resource_type`,
  `AccountUpsert::resource_type` — тип ресурса бронирования; правило `meta::account_resource_type` (ресурс без типа — `Room`, тип у
  пользователя — `Invalid`). `MemMetadata` хранит тип; набор `suite::account_resource_type`. `PgMeta` проверяет правило и читает ресурс
  как `Room` — хранение в БД (миграция) META-81; Admin API — ADM-47, CalDAV — DAV-38, JMAP — JMAP-45. Несовместимо: новые обязательные
  поля `Account`, `AccountUpsert`; все места сборки в репозитории (`tp-admin`, `tp-auth`, `tp-meta`, `tp-testkit`, тесты `tp-imap`,
  `tp-smtp`, `tp-dav`, `tp-jmap`, `thepost`) дополнены: пересборка из `Account` переносит тип, прочие — `None`.
- 2026-11-14 — CTR-90 (волна contracts-61): `sharing::MAX_NOTICES` = 1000 — предел уведомлений о доступе на учётную запись:
  `ShareNotificationStore::record` сверх него вытесняет старшие по (`created`, `id`), вытеснение — шаг состояния и `destroyed` в
  изменениях. Сигнатуры не меняются. Реализации — `MemShareNotifications`, `PgMeta` (без миграции; блокированные удалением или сроком
  хранения уведомления вытеснение пропускает — `SKIP LOCKED`); набор `suite::sharing::share_notifications` — проверка 1001-й записи.
- 2026-11-14 — ADM-41 (x/admin-27, решение 117, решение владельца 14.11 «Переделать роль»): `AccountStore::grant_installation_admin`,
  `is_installation_admin` — роль администратора установки явной записью вместо признака «действующий пароль приложения с подписью
  `thepost admin init`». Без реализации по умолчанию: `PgMeta` (таблица `installation_admins`, миграция 0040 переносит роль учётной
  записи `admin init` по прежнему признаку), `MemMetadata`; набор `suite::account_installation_admin`. Несовместимо: новые
  обязательные методы `AccountStore`.
- 2026-11-14 — CTR-86 (волна contracts-58): `sharing::ShareNotificationStore` — хранилище уведомлений о доступе (JMAP
  `ShareNotification`) вместо памяти узла `tp-jmap` (хвост jmap-19, #737): запись, список, удаление, изменения по состоянию, срок
  хранения; типы `ShareNotification`, `ShareChangedBy`, `SharedObjectType`, `ShareNotificationFilter`, `ShareNotificationChanges`,
  идентификатор `ShareNotificationId`. Все методы обязательные (реализаций вне `tp-testkit` пока нет). In-memory —
  `MemShareNotifications`, набор `suite::sharing::share_notifications`. По сравнению с карточкой: `MemShareNotifications` — в этой волне
  (иначе набору не на чем пройти); вытеснение сверх `MAX_NOTICES` в контракт не входит — только срок хранения. `tp-meta` (миграция —
  номер у оркестратора) и перевод JMAP — отдельные волны.
- 2026-11-14 — CTR-87 (волна contracts-58): `Directory::contacts_since` — без реализации по умолчанию (было «каталог без контактов»:
  прятало реализацию, забывшую контакты). Реализации — `LdapDirectory`, `PgMeta` (META-80), `FakeDirectory` — уже были, других
  реализаторов нет. Несовместимо: новый обязательный метод `Directory`.
- 2026-11-13 — CTR-84 (волна contracts-56): `sharing::SharingAuthorizer` — проверка прав на коллекции для протоколов (`rights`, `check`,
  `shared_with`, `delegations`, `check_grant`, `check_delegate`), все методы обязательные. Новый трейт, а не расширение `Authorizer`: у
  папок и коллекций разные хранилища и реализации, CalDAV и CardDAV права на папки не нужны. `tp_auth::CollectionAuthorizer` реализует
  трейт (собственных методов с теми же именами больше нет); узел — `Services::collections` поверх `PgMeta`. `tp-testkit`: набор
  `suite::sharing_authorizer` (проверки AUTH-12) — прогон в `tp-auth` на `MemDav` и `MemMetadata` вместо временных хранилищ тестов;
  `FakeSharingAuthorizer`. Несовместимо: вызовы методов `CollectionAuthorizer` требуют трейта в области видимости (вызовов вне
  `tp-auth` не было).
- 2026-11-14 — CTR-85 (волна contracts-57): `QueueStore::list` и `QueueStore::retry` — просмотр очереди организации порциями с фильтром
  по состоянию и адресу (`QueueFilter`, `QueueItem`, `QueueItemRcpt`, `QueueState`, `QueueRcptState`) и повтор доставки «сейчас» с
  сигналом `QueueReady`. Методы по умолчанию — `Internal` до реализации в `tp-meta` (отдельная волна, миграция не нужна: статусы
  получателей и лиза уже в `queue`, `queue_rcpts`). In-memory — `MemQueue` (хранит конечный статус получателя); набор
  `suite::lifecycle::queue_list`. По сравнению с карточкой: оба метода принимают `now` (время передаёт вызывающий, как у `claim`);
  `retry` возвращает `bool` (`false` — запись под лизой); `MemQueue` реализует методы в этой волне — иначе набору не на чем пройти.
- 2026-11-14 — CTR-91 (x/contracts-62, решение 122): `branding::{BrandingStore, Branding, BrandingLevels, EffectiveBranding,
  BrandingScope, BrandColor, BrandLogo, Theme, Role, CliRole, Palette, ContrastPair, ContrastIssue, OrgContrastIssue,
  installation_change_issues, CONTRAST, CLI_BACKGROUNDS,
  CLI_MIN_CONTRAST, MAX_PRODUCT_NAME, MAX_LOGO_SIZE, MAX_LOGO_SIDE}` — брендинг заказчика: все роли `design/tokens.toml`, обязательных
  полей нет, незаданное — с уровня выше (организация → установка → IT TASK), вывод ролей из основного цвета, контраст без исключений, проверка смены установки по организациям, страховка
  отрисовки;
  раздел «Branding». In-memory — `MemBranding`; набор `suite::branding`. PostgreSQL — META-84, Admin API — ADM-48.
