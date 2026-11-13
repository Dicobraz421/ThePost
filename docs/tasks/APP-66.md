# APP-66: шина узла — обработчику очереди smtp-out

lane: app
deps: [CTR-73]
class: A
files: [crates/thepost/src/services.rs]
read: [docs/tasks/SMTP-42.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 74: smtp-07, PR #548. Подготовка к SMTP-42: smtp-out будет ждать сигнала
`QueueReady` вместо опроса очереди, а у `RoleCtx` шины нет.

## Сделать
- `Services::queue_worker`: `QueueWorker::with_routes(routes, Some(bus))` вместо `None` — обработчик получает шину
  узла; описание метода — о шине для сигналов очереди.
- Пока опрос очереди не удалён (SMTP-42), поведение не меняется. До SMTP-42 роль smtp-out дополнительно следит
  за настройками транспорта сама (`Routes::watch` в `SmtpOutService::run`) рядом с общим наблюдателем `serve`:
  повторная пометка снимка устаревшим безвредна. SMTP-42 убирает этот запуск из роли.

## Приёмка
- CI зелёный.
