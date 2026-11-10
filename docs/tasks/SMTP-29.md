# SMTP-29: переписывание адресов из данных организации; `postpone` в планировщике

lane: smtp
deps: [CTR-40, SMTP-13, SMTP-14]
class: A
files: [crates/tp-smtp/src/*]
read: []
checks: [nextest]
manual: no

Отчёты smtp-04 и contracts-28 (CTR-40).

## Сделать
- Таблица переписывания — `TenantTransport::rewrites` (снимок транспорта, обновление по `TransportChanged`) вместо
  `smtp.addressing.rewrite`; ключ конфигурации — ошибка разбора с подсказкой «Admin API».
- Планировщик очереди переносит получателей через `QueueStore::postpone` (без увеличения `attempts`).
- Тесты на фейках testkit.
