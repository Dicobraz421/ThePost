# BLOB-11: конфиг tp-blob из config.yaml, ключи S3 из хранилища секретов

lane: blob
deps: [BLOB-09, CTR-13, CTR-15]
class: A
files: [crates/tp-blob/src/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов, docs/decisions/2026-10-30-config-events-secrets.md#Длительности в конфигурации (#43, вариант 2)]
checks: [nextest]
manual: no

## Сделать
- `Deserialize` для секций tp-blob (локальный, S3, кэш, GC, сироты); длительности — строки humantime (`tp_contracts::config::duration`), поля без единицы.
- Ключи S3 — только ссылки `{ secret: "<путь>#<поле>" }` через `SecretStore`; значение открытым текстом — ошибка конфигурации.
- `GcWorker::run` и `OrphanSweeper::run` — `Send`-футуры (приложение сейчас обходит через `spawn_blocking`).

## Приёмка
- Тест разбора секций из YAML; ключ S3 открытым текстом отклоняется; фоновые задачи запускаются через `tokio::spawn`.
