# META-05: LeaseStore

lane: meta
deps: [META-01, CTR-04]
class: A
files: [crates/tp-meta/src/lease.rs]
read: [docs/CONTRACTS.md#Lease]
checks: [nextest]
manual: no

## Сделать
- Лизы на таблице leases (name, holder, until), продление и освобождение.

## Приёмка
- Тест: второй претендент не получает лизу до истечения.
