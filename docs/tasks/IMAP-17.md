# IMAP-17: IDLE — полная сверка по `Resync`

lane: imap
deps: [CLU-13]
class: A
files: [crates/tp-imap/src/*]
read: []
checks: [nextest]
manual: no

Отчёт cluster-08 (CLU-13): шина шлёт `Resync` при отставании подписчика (и после переподключения шины PG), а IDLE
(`idle.rs`, ветка `_ => false`) его игнорирует — уведомления теряются.

## Сделать
- `Resync` своей организации/учётной записи или системный → `sync()` выбранной папки (EXISTS/EXPUNGE/FETCH FLAGS по
  разнице состояний).
- Тест на фейке шины: `Resync` вместо событий — клиент IDLE получает изменения.
