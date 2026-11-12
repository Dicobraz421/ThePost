# ADM-21: очередь модерации — страницы по курсору

lane: admin
deps: [CTR-55, META-59]
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны console-02, PR #439.

## Сделать
- `GET /api/v1/orgs/{org}/moderation` (`groups.rs::list_held`): параметр `after` и продолжение в заголовке `Link`
  (`rel="next"`) — как у списка учётных записей; страница — `GroupStore::held_after` (CTR-55), размер — `limit` в
  пределах секции `admin`. Формат элементов (`HeldInfo`) не меняется.

## Приёмка
- Тест роутера: очередь из 5 писем при `limit=2` читается тремя запросами по ссылке `Link` без пропусков и повторов.
