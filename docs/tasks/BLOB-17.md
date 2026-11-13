# BLOB-17: `verify`: раздельные счётчики «пропал» и «неверный размер»

lane: blob
deps: [BLOB-14]
class: A
files: [crates/tp-blob/src/*]
read: [docs/tasks/APP-44.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 68: app-39, PR #530.

## Сделать
- `VerifyReport` (`verify.rs`): отдельные `missing` и `size_mismatch` вместо общего счётчика; вывод `thepost verify` и
  метрики APP-44 — по ним.

## Приёмка
- Тест: удалённый объект и объект с другим размером попадают в разные счётчики.
