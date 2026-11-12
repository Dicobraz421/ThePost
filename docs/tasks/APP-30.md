# APP-30: cluster.listen — адрес приёма межузловых вызовов отдельно от объявляемого

lane: app
deps: [APP-18]
class: A
files: [crates/thepost/src/*, deploy/config/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Ключ `cluster.listen` (адрес:порт или `0.0.0.0:порт`); по умолчанию — как `cluster.addr` (решение 36). Узел слушает
  `listen`, другим узлам объявляет `addr`. Проверка в `doctor`/при старте: порт задан.

## Приёмка
- Тест: узел слушает `listen`, второй вызывает его по `addr` через проброс порта (или два адреса одного хоста).
