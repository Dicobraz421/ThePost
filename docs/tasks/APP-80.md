# APP-80: `thepost admin tui` правит config.yaml узла; ключ `--theme`

lane: app
deps: [TUI-04, PLT-35, TUI-11]
class: A
files: [crates/thepost/src/cli.rs]
read: [docs/tasks/TUI-04.md, docs/tasks/TUI-05.md, docs/tasks/PLT-35.md, docs/decisions/2026-11-11-stage2-followups.md#118]
checks: [nextest]
manual: yes

## Сделать
- `thepost admin tui` заполняет `Connect.node`, когда файл `--config` (или `THEPOST_CONFIG`, или
  `/etc/thepost/config.yaml`) есть: интерфейс запущен на самом узле. Нет файла — `None`, раздел «Конфигурация
  узла» поясняет, где её править.
- `ConfigHost` узла: схема — `sections::registry()` (как `thepost config schema`); YAML ↔ JSON —
  `tp_platform::config::yaml_to_json` / `json_to_yaml` (PLT-35, разбор загрузчика); проверка — `cli::config_check`
  без цвета, строки ошибок без знака — `путь.параметра: причина`, как ждёт `tp_tui::node::save`.
- Ключ `--theme auto|dark|light|256|16|mono` → `Connect.theme`; без ключа `THEPOST_TUI_THEME` остаётся значением по
  умолчанию (его читает tp-tui).

## Как делают другие
- `visudo` и `crontab -e` (https://www.sudo.ws/docs/man/visudo.man/,
  https://man7.org/linux/man-pages/man1/crontab.1.html): правка идёт через ту же проверку, что у самой программы, —
  у нас `ConfigHost::check` вызывает тот же `config_check`, что `thepost config check`, второго разбора нет.
- Тема по ключу с запасным значением из переменной окружения — как `--color` у `ls`/`git` и `NO_COLOR`
  (https://no-color.org/); у нас ключ сильнее переменной, переменная сильнее автоопределения.

## Приёмка
- Тесты: разбор `--theme` (все значения, неизвестная тема — ошибка, справка упоминает `THEPOST_TUI_THEME`);
  `ConfigHost` на временном файле — схема, YAML туда-обратно, проверка, запись через `tp_tui::node::save` (неизвестный
  параметр — ошибка у поля, верное значение записано с `.bak`).
- Ручная проверка на стенде: `thepost admin tui` на узле — правка параметра видна в `config.yaml`, узел перечитал
  файл.
