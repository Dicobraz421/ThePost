# META-55: замер ожидания соединения пула

lane: meta
deps: [CTR-51]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны platform-15, PR #434.

Подтверждено владельцем — решение 70 (`docs/decisions/2026-11-11-stage2-followups.md`).

## Сделать
- Пул узла (`pool.rs`, `failover.rs`) принимает `Arc<dyn AcquireObserver>` (CTR-51; по умолчанию — пустой): получение
  соединения в `with_tenant`, `with_system` и их `_keyed`-вариантах (`pool.acquire()`, `begin()`) идёт через наблюдателя;
  отказ и тайм-аут — исход «отказ».
- Снимок пула для `/metrics` (занято, свободно, предел) доступен узлу и при `Failover`, и при шардах.
- Имя соединения слушателя (`connect_listener`: `"<имя> listener"`) не менять — оно совпадает с
  `tp_platform::dbmetrics::listener_application_name`.

## Приёмка
- Тест на PostgreSQL (`TEST_DATABASE_URL`): пул из одного соединения, вторая транзакция ждёт — наблюдатель получает
  ожидание и его время; отменённое ожидание не оставляет «ожидает».
