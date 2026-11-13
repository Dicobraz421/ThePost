# ADM-44: клиент Admin API — общий крейт для CLI и текстового интерфейса

lane: admin
deps: []
class: B
files: [crates/tp-admin-client/*, crates/thepost/src/admin_client.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md#118]
checks: [nextest]
manual: no

## Сделать
- Вынести клиент Admin API из `crates/thepost/src/admin_client.rs` в крейт `tp-admin-client` (вход: пароль, OIDC
  device flow, токен; сессии; problem+json; листание по `Link`/`next`; типы ответов из `tp-admin`). CLI переходит
  на крейт, старый модуль удаляется.

## Приёмка
- Тесты CLI проходят на новом крейте; в `crates/thepost` нет второй копии клиента.
