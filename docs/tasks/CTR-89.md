# CTR-89: `QueueStore::list` и `retry` без тел по умолчанию

lane: contracts
deps: [META-82]
class: B
files: [crates/tp-contracts/src/queue.rs, crates/tp-testkit/src/fakes.rs, crates/tp-smtp/src/queue/tests.rs, docs/CONTRACTS.md]
read: [docs/tasks/META-82.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- У `QueueStore::list` и `QueueStore::retry` убрать тела по умолчанию (`Error::Internal`): это параллельный путь,
  реализация без просмотра очереди не нужна. Метод становится обязательным.
- Реализаторов поправить в этом же PR: `MemQueue` (`tp-testkit`), заглушки `NoPostpone` и `Counted` в
  `tp-smtp` (передают вызов вложенному хранилищу), заглушку `Bare` в тесте контракта; тест `list_retry_default_internal`
  удалить. `PgMeta` реализует методы с META-82. Описание в `docs/CONTRACTS.md` — без слов «по умолчанию —
  `Internal`».

## Приёмка
- Рабочее дерево собирается без реализаторов `QueueStore`, у которых нет `list` и `retry`; поиск
  `does not support list` пуст; `nextest` проходит.
