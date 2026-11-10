# CMP-07: отправка из JMAP через `Delivery` — локальным адресатам сразу

lane: app
deps: [CMP-04, CMP-08]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёт smtp-08 (CMP-04): в tp-smtp есть `Delivery::submit` и `impl Outbound for Delivery` (локальные — сразу через `ingest`,
внешние — в очередь, статус по получателю), но `services.rs::jmap()` передаёт JMAP очередь (`self.queue`), а не `Delivery`.

## Сделать
- `JmapOutbound` оборачивает `Arc<Delivery>` из `self.delivery(smtp)?` (в `jmap()` нужен `SmtpConfig`), а не очередь.
- Статусы по получателям — в метод `tp_jmap::Outbound` со статусами (CMP-08) → `deliveryStatus` EmailSubmission.
- Внешний адресат без подключённой очереди — ошибка только для внешних, локальные доставлены (сейчас вся отправка — `Unavailable`).
- Тест сборки сервисов: роль `all`, отправка из JMAP локальному адресату без DNS — письмо во «Входящих» получателя.
