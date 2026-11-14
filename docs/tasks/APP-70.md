# APP-70: подсветка справки всех команд `thepost`, цветные `doctor` и `config check`

lane: app
deps: [ADM-46]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#120, docs/tasks/ADM-46.md]
checks: [nextest]
manual: no

## Сделать
- Справка всех команд `thepost` — `clap::builder::Styles` по ролям из `tp-admin-client::style`: разделы («Команды:»,
  «Параметры:», «Использование:») — `head`, подкоманды и флаги — `accent`, `[по умолчанию: …]` — `muted`; ошибки
  разбора аргументов — `err`. Своих цветов в `crates/thepost` нет.
- `thepost doctor`: строки со знаком и меткой — ✓ `OK` (`ok`), ▲ `WARN` (`warn`), × `FAIL` (`err`); пути файлов —
  `muted`; итог «итог: 1 ошибка, 1 предупреждение» с цветом по худшему состоянию.
- `thepost config check`: тот же вид — ✓ при успехе, × и путь к полю при ошибке.
- `--color auto|always|never` у `doctor` и `config check` — тот же разбор и выбор, что у `thepost admin`
  (`tp-admin-client::style`); справка учитывает тот же выбор и переменные `NO_COLOR`, `CLICOLOR_FORCE`, `TERM=dumb`.
  Данные — в stdout, ошибки — в stderr; коды возврата не меняются.

## Приёмка
- Тесты снимков (`insta`) `doctor`, `config check` и `thepost admin accounts --help` в цвете и без цвета; знаки ✓ ▲ ×
  есть и без цвета.
- Тест: вывод `doctor` в конвейер при `auto` не содержит `\x1b`.
