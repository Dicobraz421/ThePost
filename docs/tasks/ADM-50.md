# ADM-50: удержание беседы в Admin API — удержать, снять, перечислить, выгрузить; CLI

lane: admin
deps: [CTR-94, ADM-44]
class: B
files: [crates/tp-admin/*, crates/tp-admin-client/*, crates/thepost/src/admin_cli.rs, console/src/api/openapi.json, console/src/api/schema.d.ts]
read: [docs/decisions/2026-11-11-stage2-followups.md#125, docs/tasks/CTR-94.md, crates/tp-admin/src/server.rs]
checks: [nextest, console]
manual: no

## Сделать
- Класс B: схема Admin API меняется — копия консоли `console/src/api/openapi.json` и `schema.d.ts` в этом же PR.
- Маршруты (администратор организации и установки; `GET` — и оператор):
  `GET /api/v1/orgs/{org}/accounts/{account}/threads?message_id=…` — беседа письма по Message-ID;
  `PUT|DELETE /api/v1/orgs/{org}/accounts/{account}/threads/{thread}/hold` — удержать (тело — основание) и снять;
  `GET /api/v1/orgs/{org}/holds` — удержанные беседы; `GET …/threads/{thread}/export` — выгрузка беседы (письма EML в
  архиве ZIP потоком, в том числе удалённые в корзину и скрытые).
- Каждое удержание, снятие и выгрузка — в журнал аудита (кто, беседа, основание; без содержимого писем).
- CLI — `thepost admin thread find <учётка> --message-id <id>`, `thread hold|unhold <учётка> <беседа> [--reason …]`,
  `thread holds`, `thread export <учётка> <беседа> -o <файл.zip>`; `tp-admin-client` — те же методы.

## Как делают другие
- Google Vault: удержание одного письма треда удерживает весь тред (https://support.google.com/vault/answer/2535539).
- Purview eDiscovery строит свои цепочки для выгрузки (https://learn.microsoft.com/en-us/purview/ediscovery-email-threading);
  Proofpoint Discover и Supervision — просмотр по тредам.
- Отличие: удержание беседы — в базовой поставке; политики хранения — модуль «Соответствие требованиям» через хук.

## Приёмка
- Тесты: удержание беседы — письмо, пришедшее после, тоже удержано; `before_final_delete` писем беседы — `Deny`;
  снятие — снова по сроку корзины; выгрузка содержит все письма беседы, включая скрытые; записи аудита есть.
- Права: администратор организации не видит беседы чужой организации (404); оператор — только `GET`.
