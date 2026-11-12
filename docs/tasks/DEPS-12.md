# DEPS-12: фича `gssapi` в сборке узла

lane: deps
deps: [AUTH-13]
class: B
files: [Cargo.toml, Cargo.lock, crates/thepost/Cargo.toml]
read: [docs/spec/S6.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 54: app-26, PR #491. входа по Kerberos (GSSAPI) в собранном узле нет: фича не проброшена в `thepost`.

## Сделать
- Фича `thepost/gssapi` → `tp-auth/gssapi` (`libgssapi` уже в workspace); в выпуске и бете — включена (CI-12).

## Приёмка
- `cargo build -p thepost --features gssapi` в CI; `cargo deny` без новых записей.
