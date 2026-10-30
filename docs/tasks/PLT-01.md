# PLT-01: Загрузка конфигурации

lane: platform
deps: [CTR-04]
class: A
files: [crates/tp-platform/src/config.rs]
read: [docs/spec/S5.md#5.5, docs/spec/S2.md#2.1, docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Чтение config.yaml (serde_yaml_ng) в дерево секций; переопределение переменными THEPOST_*; три обязательных параметра;
- параметр roles: пресеты и роли (RolePreset); секции modules.<имя> с enabled; реестр секций крейтов без центрального перечня;
- JSON Schema конфигурации (schemars) — проверка при загрузке и hot reload, вывод `thepost config schema`.

## Приёмка
- Минимальный файл из трёх строк загружается; ошибка указывает путь параметра; схема покрывает все секции.
