# FIX-02: узел: решения модерации через `tp-smtp` вместо `NoModeration`

lane: app
deps: [FIX-01]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/FIX-01.md]
checks: [nextest]
manual: no

**Срочно** — ошибка на живом узле (проверка консоли на стенде), этап 2, первой в «Порядке работ».

Хвост реестра облачных волн, п. 26: console-02, PR #453.

## Сделать
- `services.rs`: Admin API получает реализацию `Moderation` из FIX-01 (доставка узла, очередь); `NoModeration` удалить.

## Приёмка
- Интеграционный тест узла: письмо в модерируемую группу → `POST …/moderation/{id}/approve` — 200, письмо у участников;
  `reject` — 200, DSN отправителю в очереди.
