# APP-74: узел — очередь исходящей почты подключена к Admin API

lane: app
deps: [META-82, ADM-45]
class: A
files: [crates/thepost/src/services.rs, crates/thepost/tests/e2e.rs]
read: [docs/tasks/ADM-45.md, docs/tasks/META-82.md]
checks: [nextest]
manual: no

## Сделать
- `Services::admin` подключает очередь: `.with_queue(Arc::new(self.meta.clone()))` (`QueueStore` поверх `PgMeta`).
  Без этого `GET /orgs/{org}/queue` и `POST …/retry` отвечают 503.
- e2e узла: `GET /api/v1/orgs/{org}/queue` отвечает 200 и отдаёт JSON-массив.

## Приёмка
- Тест e2e проходит в CI (PostgreSQL); `GET …/queue` — 200, не 503.
