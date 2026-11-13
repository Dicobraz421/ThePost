# PLT-34: адрес клиента — в канонической форме при приёме соединения

lane: platform
deps: []
class: A
files: [crates/tp-platform/src/listen.rs]
read: [docs/tasks/AUTH-19.md]
checks: [nextest]
manual: no

## Сделать
- `Incoming::establish_inner` кладёт в `ConnInfo.client` сырой адрес от двухстекового `[::]` — IPv4 приходит как
  `::ffff:a.b.c.d` во все протоколы (отчёт auth-14, #679). AUTH-19 нормализует его только в `AbuseGuard`.
- Приводить к `to_canonical()` в одном месте — при приёме соединения (и для адреса из PROXY). После этого нормализации
  в потребителях (AUTH-19, tp-smtp, ratelimit) — удалить, чтобы не было второго пути.

## Приёмка
- Тест: соединение по IPv4 через `[::]` даёт `ConnInfo.client` вида `10.0.0.1`; PROXY v2 с IPv4 — так же.
