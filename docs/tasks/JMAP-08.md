# JMAP-08: Push и SearchSnippet

lane: jmap
deps: [JMAP-04, CLU-01]
class: A
files: [crates/tp-jmap/src/push.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- EventSource и WebSocket (RFC 8887), StateChange по EventBus; SearchSnippet/get.

## Приёмка
- Тест: изменение приходит в WebSocket.
