# CLU-02: Консистентное хэширование

lane: cluster
deps: []
class: A
files: [crates/tp-cluster/src/hashing.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Rendezvous hashing account → узел из списка участников; минимальные перемещения при изменении состава.

## Приёмка
- Тест: при удалении узла перемещаются только его ключи.
