# DEPS-11: `calcard` в dev-зависимостях `tp-testkit`

lane: deps
deps: [KIT-09]
class: B
files: [Cargo.toml, Cargo.lock, crates/tp-testkit/Cargo.toml]
read: [docs/tasks/KIT-09.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 41: testkit-06, PR #482. тест разбора корпуса vCard библиотекой нужен в самом testkit — иначе ошибка в
образце всплывёт в чужой полосе.

## Сделать
- `calcard` (уже в workspace) — dev-зависимость `tp-testkit`; новых внешних пакетов нет.

## Приёмка
- `cargo deny check` без новых записей; CI зелёный.
