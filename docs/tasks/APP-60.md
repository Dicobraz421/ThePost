# APP-60: thepost doctor: подсказки DNS для календарей и контактов

lane: app
deps: [DEP-21]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `doctor` проверяет записи `_caldavs._tcp`, `_carddavs._tcp` (SRV) и `_caldavs`/`_carddavs` TXT `path=` для каждого
  домена; при отсутствии — `WARN` с готовой строкой записи для DNS (отчёт deploy-17, #612).

## Приёмка
- Тест: домен без SRV — предупреждение с текстом записи; с записями — `OK`.
