# CTR-84: контракт проверки прав на коллекции — `SharingAuthorizer`

lane: contracts
deps: [CTR-48]
class: B
files: [crates/tp-contracts/src/sharing.rs, crates/tp-contracts/src/auth.rs, crates/tp-auth/src/sharing.rs, crates/tp-auth/src/lib.rs, crates/tp-testkit/src/*, crates/thepost/src/services.rs, docs/CONTRACTS.md]
read: [docs/tasks/AUTH-12.md, docs/tasks/META-52.md, docs/tasks/DAV-17.md, docs/tasks/DAV-18.md, docs/tasks/JMAP-32.md]
checks: [nextest]
manual: no

## Сделать
- AUTH-12 (#735) добавил `tp_auth::CollectionAuthorizer` — права на календари и адресные книги (владелец, записи по
  субъектам и группам, `anyone`, представители, «только занятость», политика организации), но трейт
  `tp_contracts::auth::Authorizer` знает только папки. Протоколы (`tp-dav`, `tp-jmap`) от `tp-auth` не зависят, поэтому
  DAV-17/18 и JMAP-32 проверить права на коллекцию не могут, а считать их внутри протокола — второй путь рядом с AUTH-12.
- Трейт `tp_contracts::sharing::SharingAuthorizer`: `rights`, `check`, `shared_with`, `delegations`, `check_grant`,
  `check_delegate` — сигнатуры `CollectionAuthorizer`, все обязательные, без тел по умолчанию.
- Новый трейт, а не расширение `Authorizer`: у папок и коллекций разные хранилища и реализации (`AclAuthorizer` поверх
  `AclStore`, `CollectionAuthorizer` поверх `SharingStores`); расширение заставило бы `AclAuthorizer` и все фейки
  `Authorizer` протоколов почты реализовать права на коллекции, а CalDAV и CardDAV не нужны права на папки.
- `tp-auth`: `CollectionAuthorizer` реализует трейт, прежние собственные методы — только в реализации трейта.
- `tp-testkit`: набор `suite::sharing_authorizer` (проверки из тестов AUTH-12) — прогон в `tp-auth` на
  `CollectionAuthorizer` поверх `MemDav` и `MemMetadata` (META-52, #736) вместо временных хранилищ в модуле тестов;
  фейк `FakeSharingAuthorizer` (права по таблице) — для тестов протоколов.
- Узел: `Services::collections` — `CollectionAuthorizer` поверх `PgMeta`; CalDAV, CardDAV и JMAP подключают своими волнами.

## Приёмка
- Набор `suite::sharing_authorizer` проходит на `CollectionAuthorizer` + `Mem*`; CHANGELOG в CONTRACTS.md.
