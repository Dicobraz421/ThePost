# APP-31: старт узла при медленном OpenBao

lane: app
deps: []
class: A
files: [crates/thepost/src/*, deploy/systemd/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Вход в OpenBao при старте — с повторами и растущей паузой до `secrets.startup_timeout` (по умолчанию 120 с),
  в журнале — причина и следующая попытка; юнит systemd — без быстрых циклов перезапуска (решение 58).

## Приёмка
- Тест: OpenBao отвечает через 20 с — узел стартует без перезапуска.
