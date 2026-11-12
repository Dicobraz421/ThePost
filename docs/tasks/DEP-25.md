# DEP-25: стенд Д.6 — два прогона, квота 150 MB, генератор доверенный, сценарий перебора

lane: deploy
deps: [DEP-14]
class: A
files: [deploy/stand/*, docs/manual-tests/DEP-25.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/manual-tests/DEP-14.md]
checks: [none]
manual: yes

## Сделать
- `stand_profile: speed` (по умолчанию) — рабочие пороги, критерии Д.7; `stand_profile: thresholds` — пороги Д.5,
  критерий «не падает, отказы только 4xx, принятые письма не потеряны» (решение 54).
- Квота ящика стенда 150 MB (решение 55); генератор — доверенный адрес на время замеров; сценарий `bruteforce` со
  второго адреса проверяет защиту от перебора (решение 56).
- `criteria.py` берёт новые метрики META-53, BLOB-16, PLT-25, когда они есть.

## Приёмка
- Оба профиля и `bruteforce` отрабатывают на уменьшенном стенде, отчёт — `docs/manual-tests/DEP-25.md`.
