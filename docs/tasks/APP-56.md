# APP-56: узел: OIDC с ключом из хранилища секретов

lane: app
deps: [AUTH-17]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/AUTH-17.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 76: auth-12, PR #541. без `.with_secrets` OIDC с `tls.key` (mTLS к поставщику) не стартует.

## Сделать
- `services.rs`: после `OidcAuthenticator::new(...)` — `.with_secrets(secrets).await` (как у LDAP).

## Приёмка
- Интеграционный тест узла: OIDC с ключом клиента из хранилища в памяти стартует.
