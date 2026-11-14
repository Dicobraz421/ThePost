# APP-72: узел — `ShareNotification` переживают перезапуск и видны на другом узле

lane: app
deps: [JMAP-46]
class: A
files: [crates/thepost/tests/*]
read: [docs/tasks/JMAP-46.md, docs/tasks/META-83.md]
checks: [nextest]
manual: no

## Сделать
- e2e узла на PostgreSQL: выдача прав записывает получателю `ShareNotification`; после перезапуска узла запись
  читается через JMAP `ShareNotification/get`; второй узел с той же базой видит её же; `/set` с `destroy` на одном
  узле убирает запись на другом.

## Приёмка
- Тест e2e проходит в CI (PostgreSQL); правки только в тестах.
