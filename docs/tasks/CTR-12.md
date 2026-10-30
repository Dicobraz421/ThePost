# CTR-12: TLS внутренних каналов и неизменяемость сегментов в контракте

lane: contracts
deps: [CTR-10]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- Общая секция конфигурации TLS внутреннего канала (CA, клиентский сертификат и ключ, имя сервера, `allow_plaintext`): одна для PostgreSQL, Valkey, S3, узел↔узел.
- Правило: `allow_plaintext` разрешён только в профиле dev; иначе адрес без TLS — ошибка конфигурации; при открытом канале — предупреждение в лог на каждом запуске.
- В CONTRACTS.md закрепить: сегменты индексов (`*_raw`) неизменяемы, перезапись запрещена.

## Приёмка
- Тест: `allow_plaintext` вне dev отклоняется с понятной ошибкой.
