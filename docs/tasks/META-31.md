# META-31: журнал событий, очередь и учёт BLOB по шардам

lane: meta
deps: [META-30, CTR-34]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/spec/S3.md#3.6, docs/spec/S3.md#3.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Журнал событий — на шарде ящика, разделы `Partition { shard, index }`; сигналы — со всех шардов (слушатель на primary каждого шарда).
- Очередь исходящей почты — на шарде отправителя (атомарность с «Отправленными»); `claim` обходит шарды по очереди.
- Учёт ссылок BLOB — на шарде; объект подлежит удалению, только когда ссылок нет ни на одном шарде.

## Приёмка
- Тесты на двух шардах: события и задания очереди обоих шардов обрабатываются; объект, на который ссылается второй шард, не удаляется.
