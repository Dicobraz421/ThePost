# CTR-57: адрес клиента в проверке учётных данных

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/spec/S6.md#6.3, docs/CONTRACTS.md#Auth, docs/CONTRACTS.md#SASL]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: счётчики по адресу и подсети не ведутся — контракт `Authenticator::verify(Credentials)` и
`SaslContext` не несут адрес клиента.

Решение владельца 2026-11-12 (PR #503): один путь — `Authenticator::verify(credentials, peer)` и `SaslContext::peer`; методов
`verify_from`/`start_from` и вариантов без адреса нет. Все реализации и вызовы переведены в том же PR, протоколы передают адрес
соединения.

## Сделать
- `SaslContext`: поле `peer: Option<IpAddr>` (адрес клиента соединения); `Authenticator`: метод с адресом, например
  `verify_from(credentials, peer: Option<IpAddr>)`, по умолчанию вызывает `verify` — реализации без защиты от перебора
  не меняются. Форма — на усмотрение PR, с записью в CHANGELOG.
- Места других крейтов, где `SaslContext` собирается перечислением полей, — `peer: None` в этом же PR (класс B);
  передачу настоящего адреса делают волны полос (AUTH-14, SMTP-35, IMAP-19, JMAP-37, DAV-26, SIEVE-12).
- testkit: фейк проверки запоминает переданный адрес; `docs/CONTRACTS.md` — разделы Auth и SASL (SRS 6.3: счётчики по
  адресу, подсети и учётной записи).

## Приёмка
- CI зелёный; тест testkit: адрес из `SaslContext` доходит до `verify_from`.
