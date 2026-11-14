# DOC-09: INSTALL — ключи поиска принципалов `dav.principal_search_scan`, `dav.principal_search_results`

lane: docs
deps: [DAV-19]
class: A
files: [docs/INSTALL.md]
read: [docs/tasks/DAV-19.md]
checks: []
manual: no

## Сделать
- Таблица ключей `dav` в `docs/INSTALL.md`: `dav.principal_search_scan` (`10000`) — сколько учётных записей
  просматривает один поиск принципалов (`principal-property-search`, `calendarserver-principal-search`: выбор
  участников и переговорных в клиенте), остальные не ищутся; `dav.principal_search_results` (`50`) — сколько найденных
  принципалов в одном ответе (клиент может запросить меньше — `cs:nresults`; сверх — ответ `507` с неполным списком).
- Когда увеличивать: в организации больше `principal_search_scan` учётных записей — иначе часть сотрудников и
  переговорных не находится.

## Приёмка
- Оба ключа есть в таблице с умолчаниями, совпадающими с `tp_dav::Config::default()`.
