# CTR-28: Outbound в контрактах

lane: contracts
deps: [SMTP-06]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-smtp/src/*, crates/tp-jmap/src/*]
read: []
checks: [nextest]
manual: no

## Сделать
- Трейт постановки исходящего письма в очередь (сейчас `tp_smtp::deliver::Outbound` и копия `tp_jmap::Outbound`, PR #164) — в tp-contracts; tp-smtp и tp-jmap — на него (ветка `x/contracts-16`). Подпись DKIM остаётся в реализации (`Queue::with_dkim`).

## Приёмка
- Одно объявление трейта; tp-smtp и tp-jmap собираются и проходят тесты.
