# PLT-12: клиент OpenBao/Vault, секреты в конфигурации

lane: platform
deps: [CTR-15, CTR-12, PLT-09]
class: B
files: [crates/tp-platform/src/*, crates/tp-platform/Cargo.toml]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов, docs/spec/S6.md#6.2]
checks: [nextest]
manual: docs/manual-tests/PLT-12.md

## Сделать
- HTTP-клиент с rustls (`reqwest`) — отдельным PR `x/platform-dep`.
- `SecretStore` на API OpenBao/Vault: KV v2, аренды динамических секретов с продлением; вход — Kubernetes, сертификат TLS
  узла, AppRole (`secret_id` из файла); только TLS с CA из секции канала; секция `secrets` с `Default`.
- Разрешение `SecretRef` при загрузке и перечитывании конфигурации; значение секрета в открытом виде — ошибка строгой и мягкой проверки.
- `thepost doctor`: доступ к хранилищу, вход, срок аренды токена узла.

## Приёмка
- Тесты разбора и разрешения на `MemSecretStore`; интеграционный тест с OpenBao dev при `TEST_OPENBAO_URL`.
