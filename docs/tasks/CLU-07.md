# CLU-07: конфиг шины из config.yaml

lane: cluster
deps: [CLU-04, CLU-06, CTR-13, CTR-15]
class: A
files: [crates/tp-cluster/src/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Длительности в конфигурации (#43, вариант 2), docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов]
checks: [nextest]
manual: no

## Сделать
- `Deserialize` для конфигов шины и распределителя разделов (зависимость serde — отдельным PR `x/cluster-dep`); длительности — строки humantime.
- Пароль Valkey — только ссылкой на секрет.

## Приёмка
- Тест разбора секций из YAML.
