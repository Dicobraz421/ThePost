# PLT-35: YAML ↔ JSON для правки config.yaml вне загрузчика

lane: platform
deps: []
class: A
files: [crates/tp-platform/src/config.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md#118, docs/tasks/TUI-04.md]
checks: [nextest]
manual: no

## Сделать
- Открытые функции `tp_platform::config::yaml_to_json(&str) -> Result<serde_json::Value>` и
  `json_to_yaml(&serde_json::Value) -> Result<String>` на уже имеющемся у крейта `serde_yaml_ng` (`Cargo.lock` не
  меняется). Нужны `ConfigHost` подкоманды `thepost admin tui` (APP-78): у текстового интерфейса нет своего разбора
  YAML.
- Разбор — в то же дерево `serde_yaml_ng::Value`, что у загрузчика: повтор ключа — ошибка, как при загрузке; ошибка
  разбора — `Error::Invalid` с позицией (`at line 4 column 1`); ключ, который не выразить в JSON (`~`), — `Invalid`.

## Как делают другие
- Kubernetes (`sigs.k8s.io/yaml`, https://github.com/kubernetes-sigs/yaml): YAML переводится в JSON, дальше работа
  идёт с JSON; строгий режим (`UnmarshalStrict`) отвергает повтор ключа. У нас повтор — ошибка всегда, как у
  загрузчика `config.yaml` (иначе интерфейс молча взял бы последнее значение, а `config check` — отверг файл).
- yq (https://mikefarah.gitbook.io/yq/usage/convert): `-o=json` / `-p=json` — то же преобразование туда-обратно;
  комментарии при этом теряются. У нас так же; заголовок-комментарий файла (метка DEP-55) сохраняет `tp_tui::node::save`.

## Приёмка
- Тест туда-обратно: документ тот же, строки вида `"1"`, `"true"`, `"null"` остаются строками, вывод разбирает
  загрузчик.
- Тест ошибки разбора: `Invalid` с номером строки и столбца; повтор ключа — `Invalid`.
