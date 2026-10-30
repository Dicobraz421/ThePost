# PLT-11: TLS служебного HTTP

lane: platform
deps: [PLT-06, CTR-12]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Шифрование любого обмена, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- `/metrics` и `/healthz` — только по TLS (ACME, файлы, секреты Kubernetes с перечитыванием, как у слушателей PLT-06); открытый HTTP — только dev по явной настройке с предупреждением.

## Приёмка
- Тест: HTTPS-запрос к `/healthz/live` проходит; открытый режим вне dev — ошибка конфигурации.
