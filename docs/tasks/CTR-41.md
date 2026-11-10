# CTR-41: локальный токен первого администратора

lane: contracts
deps: [CTR-23]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Решение владельца #193: первая учётная запись SMB без OIDC — `thepost admin init` на узле.

## Сделать
- Вид учётных данных Admin API «локальный одноразовый токен» (не OIDC): выдача, проверка, срок, однократность,
  отзыв; хранение — хэш. In-memory и conformance-тест; CHANGELOG.
