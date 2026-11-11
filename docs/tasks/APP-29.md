# APP-29: календари и контакты в узле — роль dav

lane: app
deps: [DAV-08, DAV-11, DAV-12, META-48]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8]
checks: [nextest]
manual: docs/manual-tests/APP-29.md

## Сделать
- Роль `dav`: маршруты `tp_dav::router` на HTTP-слушателе — `http.dav` или, если не задан, на `http.jmap` (один порт 443 с JMAP);
  хранилище — `PgMeta` (`DavStore`), вход — через `Guarded` (защита от перебора), как у JMAP.
- `jmap.autoconfig` (`caldav`, `carddav`) указывает на этот путь; `thepost doctor` проверяет, что `/.well-known/caldav` ведёт на `/dav/`.
- Ручная проверка на стенде: подключение Thunderbird, iOS и macOS (профиль `.mobileconfig`), DAVx5 по одному адресу сервера.

## Приёмка
- Интеграционный тест узла: `PROPFIND` дома календарей с паролем приложения; сценарии ручной проверки без замечаний.
