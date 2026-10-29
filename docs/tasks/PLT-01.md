# PLT-01: Загрузка конфигурации

lane: platform
deps: [CTR-04]
class: A
files: [crates/tp-platform/src/config.rs]
read: [docs/spec/S5.md#5.5, docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Чтение config.yaml (serde_yaml_ng) в дерево секций; переопределение переменными THEPOST_*; валидация трёх обязательных параметров;
- реестр секций крейтов (имя → тип) без центрального перечня.

## Приёмка
- Минимальный файл из трёх строк загружается; ошибка указывает путь параметра.
