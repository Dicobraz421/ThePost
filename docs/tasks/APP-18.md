# APP-18: подключение транспорта организации и Rpc узла

lane: app
deps: [SMTP-29, CLU-12, APP-15]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты smtp-10 (SMTP-29) и cluster-07 (CLU-12).

## Сделать
- `Delivery::with_transport(..)`, `with_routes(..)` и `Routes::watch` (снимок транспорта организации, обновление по
  `TransportChanged`) — переписывание адресов из данных организации работает в сборке.
- Секция `cluster.rpc` (`RpcConfig`), запуск `NodeRpc::listen` на `cluster.addr`, `NodeInfo.addr` объявляется; без адреса —
  узел запросов не принимает.
- Тест сборки сервисов: переписывание адреса из `TransportStore`; два узла в процессе — вызов Rpc.
