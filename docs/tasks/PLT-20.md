# PLT-20: процессный `CryptoProvider` rustls для ACME

lane: platform
deps: [PLT-11]
class: A
files: [crates/tp-platform/src/*]
read: []
checks: [nextest]
manual: no

Отчёт app-13 (APP-14): в сборке включены оба провайдера rustls; без процессного `CryptoProvider` instant-acme паникует —
первый выпуск в `serve` роняет запуск, фоновое продление молча падает. Режим `acme` эталонной установки нерабочий.

## Сделать
- `tp_platform::Acme` (и прочие клиенты TLS крейта) ставят процессный провайдер один раз (как `install_crypto_provider`
  в `tp-cluster/valkey_bus.rs`), до первого обращения к rustls; повторная установка — не ошибка.
- `Http01Challenges::insert` — доступен для тестов вызывающего (`#[doc(hidden)]` или тестовая функция), чтобы app проверил
  ответ HTTP-01 токеном.
- Тест без dev-зависимости tokio-rustls: `Acme` создаётся и делает запрос к недоступному каталогу без паники.
