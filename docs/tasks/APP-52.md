# APP-52: maintenance: метрики очистки по сроку

lane: app
deps: [APP-32]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/APP-32.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 37: app-31, PR #455. сбой очистки надгробий DAV и журнала автоответов виден только в журнале узла.

## Сделать
- `/metrics`: `tp_maintenance_purge_{runs,errors}_total{step}`, `tp_maintenance_purged_total{step}` и время последнего
  успешного прохода для шагов очистки (корзина, надгробия DAV, журнал автоответов, журнал доставки, токены `admin init`).

## Приёмка
- Тест: ошибка шага увеличивает счётчик ошибок, успешный проход — время.
