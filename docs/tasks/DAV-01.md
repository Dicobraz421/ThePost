# DAV-01: HTTP-каркас DAV и вход

lane: dav
deps: [DEPS-04, CTR-45]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Маршрутизатор axum под префиксом `/dav/` (`tp_dav::router(Deps, DavConfig)`): `principals/<логин>/`, `calendars/<логин>/<коллекция>/`,
  `addressbooks/<логин>/<коллекция>/`; методы `OPTIONS`, `PROPFIND`, `PROPPATCH`, `REPORT`, `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`, `MKCALENDAR`.
- Вход — `Authorization: Basic` (пароль приложения) и `Bearer` через `Authenticator`, как в JMAP (`tp_jmap::core::http`); 401 с
  `WWW-Authenticate: Basic realm="dav"`; доступ только к своим коллекциям (общий доступ — DAV-17).
- Секция `dav` с `Default`: лимиты тела запроса, глубины XML, числа элементов ответа, размер объекта.
- `current-user-principal`, `principal-URL`, `displayname` принципала; ответ на корень `/dav/` и на `/.well-known/caldav|carddav`
  (редирект уже есть в tp-jmap — проверить путь).

## Приёмка
- Тесты на in-memory (`tower::ServiceExt`): вход, 401, чужой принципал — 403, путь к своему принципалу.
