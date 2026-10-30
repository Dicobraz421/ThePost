# BLOB-09: S3 только по https

lane: blob
deps: [BLOB-02, CTR-12]
class: A
files: [crates/tp-blob/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена]
checks: [nextest]
manual: no

## Сделать
- Конечная точка S3 — только `https://` (CA из секции TLS канала); `http://` — только dev по `allow_plaintext`, с предупреждением.

## Приёмка
- Тест: `http://` вне dev — ошибка конфигурации.
