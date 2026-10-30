# CTR-05: MailStore, SearchIndex, Directory, Auth

lane: contracts
deps: [CTR-02, CTR-03]
class: B
files: [crates/tp-contracts/src/mailstore.rs, crates/tp-contracts/src/search.rs, crates/tp-contracts/src/auth.rs]
read: [docs/CONTRACTS.md#MailStore, docs/CONTRACTS.md#Search, docs/CONTRACTS.md#Directory, docs/CONTRACTS.md#Auth]
checks: [nextest]
manual: no

## Сделать
- Трейт MailStore и типы IngestRequest, Delivery, Delivered; SearchIndex, IndexManifestStore, TextQuery;
- Directory (каталог: учётки, группы, алиасы, домены, атрибуты GAL); Authenticator, Authorizer, Credentials, Principal, Rights (RFC 4314).

## Приёмка
- Трейты object-safe; Rights разбирает/печатает строку `lrswipkxtea` (тест).
