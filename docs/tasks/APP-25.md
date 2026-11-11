# APP-25: thepost doctor — срок внутренних сертификатов

lane: app
deps: []
class: A
files: [crates/thepost/src/doctor.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Проверка сроков: сертификат узла, CA клиентов СУБД и OpenBao из `config.yaml` — `WARN` за 60 сут до конца, `FAIL`
  после; в пояснении — как перевыпустить (повторный прогон плейбука, DEP-18) (решение 27).

## Приёмка
- Юнит-тест на сертификатах с разными сроками.
