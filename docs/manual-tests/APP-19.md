# APP-19: `thepost admin init` — первый администратор без OIDC — ручная проверка

Нужны: эталон `--profile smb` запущен (`docs/manual-tests/CMP-01.md`, раздел «Запуск»), `curl`, `swaks`.
Ниже `DC="docker compose -f deploy/compose/dev.yml --profile smb"`, организация по умолчанию
`ORG=00000000-0000-0000-0000-000000000001`. Admin API слушает только loopback контейнера (`127.0.0.1:8443`),
в образе нет `curl`, поэтому запросы — из контейнера в сетевом пространстве `thepost`:
`ADM="docker run --rm --network container:$($DC ps -q thepost) curlimages/curl:8.11.1 -sk"`.

## Сокет
1. `$DC logs thepost | grep "локальный сокет открыт"` — путь `/var/lib/thepost/admin.sock` (`admin.init.socket`).
2. `$DC exec thepost thepost admin init --email admin@example.test` (от учётной записи службы 65532) — вывод:
   имя входа, пароль, токен `tpa_…` и срок (10 минут). Пароль и токен больше нигде не показываются.
3. Повтор той же команды — `отказ узла (409): администратор уже заведён …`, код возврата 1;
   с `--force` — новый пароль и токен, прежний пароль больше не принимается.
4. Отказ чужой учётной записи: `$DC exec -u 1000 thepost thepost admin init` — `нет доступа к сокету` (права `0600`).

## Сессия Admin API
1. `TOKEN=tpa_…` из шага 2; `$ADM -X POST -H "Authorization: Bearer $TOKEN" https://127.0.0.1:8443/api/v1/session` —
   `201`, в теле `session` (`tps_…`), `roles: ["installation_admin"]`.
2. Повтор того же запроса — `401` (токен одноразовый). Тот же токен на `/api/v1/whoami` — `401`.
3. `S=tps_…`; `$ADM -H "Authorization: Bearer $S" https://127.0.0.1:8443/api/v1/whoami` — `200`, `admin@example.test`.

## Учётные записи для CMP-01 («Письмо между двумя пользователями»)
1. Домен `example.test` уже принят (создан `admin init`):
   `$ADM -H "Authorization: Bearer $S" https://127.0.0.1:8443/api/v1/orgs/$ORG/transport/domains`.
2. `$ADM -X POST -H "Authorization: Bearer $S" -H 'Content-Type: application/json' -d '{"login":"a@example.test"}'
   https://127.0.0.1:8443/api/v1/orgs/$ORG/accounts` — `201`, запомнить `id`; то же для `b@example.test`.
3. Пароль: `$ADM -X POST -H "Authorization: Bearer $S" -H 'Content-Type: application/json' -d '{"label":"swaks"}'
   https://127.0.0.1:8443/api/v1/orgs/$ORG/accounts/<id>/app-passwords` — `password` в ответе (один раз).
4. Дальше — CMP-01, `swaks … --auth-user a@example.test --auth-password <пароль>`.

## Вход в Postman
1. Пароль администратора из шага 2 «Сокет» — вход `admin@example.test` по IMAP (993) и JMAP (443); для Admin API —
   сессия `tps_…`.

## Аудит
1. `$DC logs thepost | grep "admin init"` — записи выпуска (`201`), отказа (`409`), `--force`; в журнале нет паролей и
   токенов.
