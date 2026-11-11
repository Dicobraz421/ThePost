# SMTP-31: правила Sieve при локальной доставке

lane: smtp
deps: [CTR-46, SMTP-28]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/spec/S4.md#4.3, docs/spec/S4.md#4.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `Delivery::with_rules(Arc<dyn DeliveryRules>)`: для каждого локального получателя после правил потока и до `ingest` — `evaluate`;
  `keep`/`fileinto` → `Delivery { mailbox, flags }` (папка по имени, `:create` — создать), несколько папок — несколько доставок
  одного `ingest`; `discard` — без доставки; `redirect` — через пересылку SRS/ARC ([`crate::forward`]).
- Журнал доставки: решение Sieve (папка, пересылка, отброшено) — без содержимого письма; ошибка правил — доставка во «Входящие».
- Внутренняя почта (submission и `Delivery::submit` локальным получателям) — тоже через правила получателя.

## Приёмка
- Тесты на in-memory с `FakeRules`: папка, флаги, две папки, `discard`, `redirect` наружу и локально, ошибка правил.
