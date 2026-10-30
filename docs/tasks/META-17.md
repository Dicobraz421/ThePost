# META-17: учётные данные БД и шардов из хранилища секретов

lane: meta
deps: [CTR-15, META-08]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов]
checks: [nextest]
manual: no

## Сделать
- `PoolConfig`: адрес без пароля + `password: SecretRef`; пароль в адресе вне dev — ошибка конфигурации.
- Миграция `shards`: `dsn` без пароля, новый столбец `credentials` (`SecretRef`); строка с паролем в `dsn` отклоняется
  ограничением и проверкой при чтении справочника.
- `ShardDirectory` получает учётные данные шарда через `SecretStore`; при смене секрета (`forget`) пул пересоздаётся.

## Приёмка
- Тест на `MemSecretStore`: пул шарда подключается паролем из хранилища; отзыв секрета → понятная ошибка `Unavailable`.
