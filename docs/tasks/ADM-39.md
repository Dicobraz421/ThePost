# ADM-39: CLI: `thepost admin login --oidc`

lane: admin
deps: [APP-42]
class: A
files: [crates/thepost/src/admin_client.rs]
read: [docs/tasks/APP-42.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 67: app-39, PR #530. Код входа с устройства (`crate::oidc_device::command`) готов, но команда его не вызывает.

## Сделать
- `login --oidc` → `crate::oidc_device::command(config)`; справка CLI.

## Приёмка
- Тест CLI на заглушке поставщика: `login --oidc` получает токен.
