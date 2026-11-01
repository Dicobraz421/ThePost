# SMTP-21: принятые домены и внутренняя ретрансляция

lane: smtp
deps: [CTR-24, SMTP-03, SMTP-09]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Организация и тип домена — из `ClusterTransport`.
- Внутренняя ретрансляция: адрес вне каталога в домене `InternalRelay` принимается и ставится в очередь на `next_hop`; полномочный домен — 550 в RCPT TO.

## Приёмка
- Тесты: неизвестный адрес полномочного домена — 550; ретранслируемого — доставка на `next_hop`.
