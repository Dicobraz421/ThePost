# APP-21: CLI thepost admin на структурах tp-admin

lane: app
deps: [APP-11]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 13.

## Сделать
- `admin_client.rs`: запросы и ответы — структуры tp-admin (те же, из которых строится OpenAPI), вместо `json!(…)` и
  `serde_json::Value`; таблицы и `--json` — из типизированных ответов.
- Если нужной структуре не хватает `pub`/`Serialize` в tp-admin — описать в отчёте (полоса admin), не править.

## Приёмка
- Переименование поля в структуре запроса tp-admin ломает сборку CLI; тест сверки со спецификацией и прогон против
  роутера tp-admin — зелёные.
