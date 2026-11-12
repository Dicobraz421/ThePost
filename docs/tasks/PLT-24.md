# PLT-24: ключи TLS из OpenBao

lane: platform
deps: [PLT-23]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/decisions/2026-10-30-config-events-secrets.md]
checks: [nextest]
manual: no

## Сделать
- `tls.key` (слушатели, межузловой, клиент к СУБД) может быть ссылкой на секрет OpenBao (`{ secret: "…" }`) так же, как
  остальные секреты; перечитывание при смене версии секрета без перезапуска; файл остаётся вариантом (решение 37).
- Можно выпускать сертификаты движком PKI OpenBao — отдельной карточкой по итогам.

## Приёмка
- Тест: слушатель стартует с ключом из OpenBao (testkit), смена версии секрета перечитывается.
