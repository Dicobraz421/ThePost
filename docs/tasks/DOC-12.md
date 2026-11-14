# DOC-12: ADMIN-GUIDE — сверка раздела «Кто с кем и что может» с кодом

lane: docs
deps: [APP-76]
class: A
files: [docs/ADMIN-GUIDE.md]
read: [docs/decisions/2026-11-11-stage2-followups.md#124, docs/tasks/ADM-49.md, docs/tasks/CTR-93.md]
checks: []
manual: no

## Сделать
- Сверить раздел «Кто с кем и что может» `docs/ADMIN-GUIDE.md` с кодом после реализации: матрица по умолчанию — с
  `TrustMatrix::default()` и `HARD_CEILING` (CTR-93); маршруты и команды — с `openapi_spec()` и
  `thepost admin trust --help` (ADM-49); имена экранов — с консолью (UI-23) и текстовым интерфейсом (TUI-09); пример
  Sieve — с расширениями `tp_sieve::EXTENSIONS` (SIEVE-14).
- Убрать пометку «проектные» и строку статуса; расхождение кода с решением 124 — не правка руководства, а issue
  «решение».

## Как делают другие
- Матрица значений по умолчанию с потолками — по образцу справочников параметров Exchange и Google, на которые раздел
  ссылается (исследование `docs/research/2026-11-14-trust-matrix.md`).

## Приёмка
- Каждая клетка матрицы, маршрут и команда раздела совпадают с кодом; пример `echo … | trust set` проходит на стенде.
