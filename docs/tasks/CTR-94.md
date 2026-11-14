# CTR-94: контракты ответа и беседы — запись об отправленном, ключи меток, беседа, удержание и действия по беседе

lane: contracts
deps: [CTR-93]
class: B
files: [crates/tp-contracts/src/reply.rs, crates/tp-contracts/src/thread.rs, crates/tp-contracts/src/trust.rs, crates/tp-contracts/src/retention.rs, crates/tp-contracts/src/meta.rs, crates/tp-contracts/src/lib.rs, docs/CONTRACTS.md, crates/tp-testkit/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/research/2026-11-14-replies-threads.md, docs/tasks/CTR-93.md, crates/tp-contracts/src/retention.rs, crates/tp-contracts/src/secrets.rs]
checks: [nextest]
manual: no

## Сделать
- `reply.rs`. `SentRecord { tenant, account, tag, message_id_hash, recipients, thread, sent_at }` — запись об
  отправленном письме внешним и партнёрским адресатам; `tag` — случайное значение метки (CTR не знает формат шифра),
  `message_id_hash` — SHA-256 Message-ID клиента (решение 125 в), адресаты — нормализованные (нижний регистр домена,
  A-label). `SentLog`: `record`, `by_tag`, `by_message_id_hash`, `purge_before(t)`; срок хранения — не меньше
  `smtp.reply.max_age`. `ReplyEvidence { sent: SentRecord, age }` — итог проверки, `PeerContext { kind: PeerKind,
  reply: bool }` — для точек решения SMTP и правил Sieve: по нему `TrustMatrix` решает клетки со значением `ReplyOnly`.
- Ключи меток — `SecretRef` хранилища секретов (`secrets.rs`) с id ключа: активный подписывает, прежние проверяют до
  конца срока метки; ротация без перезапуска.
- `trust.rs` (CTR-93): `Toggle::ReplyOnly` между `Off` и `On` для `auto_reply` и `mdn`; значения по умолчанию не
  меняются.
- `thread.rs`. `ThreadId` — неизменяемый (RFC 8621 §3). `ThreadStore`: `bind(account, message, keys) -> ThreadId` —
  письмо присоединяется к беседе по `In-Reply-To`/`References` среди писем учётной записи, иначе новая беседа; беседы
  не сливаются (письмо, ссылающееся на две, — в более раннюю), `threadId` письма не меняется. Память беседы для
  аномалий — типы вложений, домены ссылок, `Reply-To` (с пределами размера из конфигурации).
- Политики беседы: `ThreadHold` (удержание на беседу целиком, включая будущие письма; кто, когда, основание) и
  `ThreadRule::{AlwaysMove(MailboxId), Ignore, AlwaysKeyword(String)}` (как EWS `ApplyConversationAction`). В
  `FinalDelete` хука `before_final_delete` (`retention.rs`) — `thread`; удержание беседы — `Deny` в базовой поставке,
  модуль «Соответствие требованиям» видит `thread` для своих правил.
- Транзакция приёма: `ThreadStore` и политики беседы — методы над транзакцией, которой пользуется `MessageStore` при
  `ingest` («письмо + беседа + применённые политики» — одна транзакция, MS-21).
- Описание — разделы «Reply» и «Thread» в `docs/CONTRACTS.md`, строка CHANGELOG. `tp-testkit`: `MemSentLog`,
  `MemThreads` и наборы `suite::reply`, `suite::thread` — их же проходит `PgMeta` (META-86).

## Как делают другие
- Запись об отправленном и признак ответа: Rspamd `replies` — хэши Message-ID исходящих, символ `REPLY` −4
  (https://docs.rspamd.com/modules/replies), `known_senders` (https://docs.rspamd.com/modules/known_senders).
- Беседа: JMAP Thread, `threadId` неизменяем (https://www.rfc-editor.org/rfc/rfc8621.html); Exchange ConversationId
  (https://learn.microsoft.com/en-us/openspecs/exchange_server_protocols/ms-oxomsg/9e994fbb-b839-495f-84e3-2c8c02c7dd9b);
  Gmail (https://developers.google.com/gmail/api/guides/threads).
- Удержание беседы целиком: Google Vault (https://support.google.com/vault/answer/2535539); действия по беседе — EWS
  (https://learn.microsoft.com/en-us/exchange/client-developer/web-service-reference/applyconversationaction-operation).
- Отличие: беседы не сливаются задним числом (RFC 8621 допускает слияние с новыми id писем) — `threadId` и удержание
  не «переезжают».

## Приёмка
- Набор `suite::reply`: запись и поиск по метке и хэшу; чужая организация не видит; `purge_before` удаляет старые.
- Набор `suite::thread`: ответ присоединяется к беседе, письмо без ссылок — новая беседа, ссылка на две беседы — в
  раннюю, `threadId` не меняется; удержание беседы распространяется на письмо, пришедшее после удержания;
  `before_final_delete` письма удержанной беседы — `Deny`.
