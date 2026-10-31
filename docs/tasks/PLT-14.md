# PLT-14: клиент OpenBao на reqwest

lane: platform
deps: [PLT-12]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-10-31-ready-made-first.md]
checks: [nextest]
manual: no

## Сделать
- `reqwest` в workspace (rustls, без default-features) — отдельный PR `x/platform-dep`.
- `VaultStore` ходит в OpenBao через `reqwest`; свой `http.rs` удалить. Интерфейс `VaultStore` не меняется.

## Приёмка
- Общий набор `tp_testkit::suite::secret_store` и интеграционный тест с OpenBao в dev-режиме проходят; `http.rs` удалён.
