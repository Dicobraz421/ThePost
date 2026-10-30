# META-14: PostgreSQL только по TLS

lane: meta
deps: [META-07, CTR-12]
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена]
checks: [nextest]
manual: no

## Сделать
- Подключение к PostgreSQL — `sslmode=verify-full` с CA из секции TLS канала (CTR-12); без TLS — только dev по `allow_plaintext`, с предупреждением.
- `thepost doctor` сообщает режим TLS подключения.

## Приёмка
- Тест разбора: `sslmode=disable` вне dev отклоняется; интеграционные тесты — в режиме dev.
