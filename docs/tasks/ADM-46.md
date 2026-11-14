# ADM-46: подсветка вывода `thepost admin` — таблицы, ошибки, изменения, JSON; модуль стилей `tp-admin-client::style`

lane: admin
deps: [ADM-44, DES-01, DEPS-16]
class: A
files: [crates/tp-admin-client/*, crates/thepost/src/admin_cli.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md#120, design/tokens.toml]
checks: [nextest]
manual: no

## Сделать
- Модуль `tp-admin-client::style`: роли `head`, `accent`, `ok`, `warn`, `err`, `muted` и роли JSON (ключ, число,
  `true`/`false`) — из раздела `[cli]` файла `design/tokens.toml` (`include_str!` при сборке, разбор `toml`; второго
  списка цветов в коде нет). По умолчанию — 16 цветов ANSI (SGR из `[cli]`), при `COLORTERM=truecolor|24bit` —
  фирменные значения. Знаки состояний ✓ ● ▲ × — тоже из модуля.
- Выбор цвета — `anstream`: `--color auto|always|never` (по умолчанию `auto`), `NO_COLOR`, `CLICOLOR_FORCE`,
  `TERM=dumb`; в файл и конвейер ESC не пишется. Данные — в stdout, ошибки и подсказки — в stderr.
- Таблицы `thepost admin`: шапка — `head`; состояния — знак и цвет (● норма, ▲ квота, × заблокирована); строка
  листания («4 из 128 · следующая страница: --page 2») — `muted`.
- Отказ Admin API (problem+json): «ошибка:» — `err`, метод и путь — `accent`, код HTTP — `err`, текст `detail`,
  строка «подсказка:» (`head`) с командой, если её можно вывести (например, `409` при создании → `… show <адрес>`).
- Изменение политик и настроек (`policies set` и прочие `set`): строки было/стало в виде `-` (`err`) и `+` (`ok`)
  только по изменённым полям, итог «✓ Сохранено · запись аудита …».
- `--json` на терминале — подсветка ключей, чисел, `true`/`false`, `null` (`muted`); в конвейере — чистый JSON, тот же
  текст байт в байт без ESC.

## Приёмка
- Тесты снимков (`insta`) вывода `accounts list`, ошибки `409`, `policies set` и `--json`: в цвете (`always`, 16 цветов
  и truecolor) и без цвета (`never`, `NO_COLOR`).
- Тест: вывод в конвейер (не терминал) при `auto` не содержит `\x1b`; `--json` разбирается `serde_json` без остатка.
