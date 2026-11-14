# APP-69: представители (`DelegateStore`) в доставке — приглашения «от имени» по `schedule_on_behalf`

lane: app
deps: []
class: B
files: [crates/thepost/src/*, crates/tp-smtp/src/*]
read: [docs/tasks/SMTP-38.md, crates/tp-contracts/src/sharing.rs]
checks: [nextest]
manual: no

## Сделать
- SMTP-38 признаёт приглашение представителя по праву отправки (Send As и Send on Behalf), а не по
  `Delegate::schedule_on_behalf`: представитель календаря без права отправки не признаётся, а право отправки без
  делегирования календаря — признаётся.
- `Delivery::with_delegates(Arc<dyn DelegateStore>)` в `tp-smtp`; проверка `SENT-BY` iMIP — по записи представителя
  организатора с `schedule_on_behalf`; проверка по правам отправки для iMIP удаляется. Узел передаёт `PgMeta` при сборке
  `Delivery` (`services.rs`).

## Приёмка
- Тесты: представитель с `schedule_on_behalf` — письмо в `ItipDelivery`; с правом Send on Behalf без делегирования —
  только письмо; e2e узла — приглашение представителя попадает в календарь.
