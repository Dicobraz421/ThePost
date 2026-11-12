# APP-26: правила Sieve и автоответ в узле — ручная проверка

Нужны: эталон `--profile smb` запущен (`docs/manual-tests/CMP-01.md`, раздел «Запуск»), администратор и сессия
`S=tps_…`, учётные записи `a@example.test` и `b@example.test` с паролями приложений (`docs/manual-tests/APP-19.md`),
`curl`, `swaks`, Postman. Обозначения `DC`, `ADM`, `ORG` — как в APP-19; `A` — `id` учётной записи `a@example.test`;
`API=https://127.0.0.1:8443/api/v1/orgs/$ORG/accounts/$A`, `H="Authorization: Bearer $S"`,
`J='Content-Type: application/json'`.

## Правило `fileinto` (Admin API)
1. Неверный скрипт: `$ADM -X PUT -H "$H" -H "$J" -d '{"text":"if header {"}' $API/sieve/scripts/main` — `422`,
   в теле `errors` со строкой и столбцом; скрипт не записан (`$ADM -H "$H" $API/sieve/scripts` — пусто).
2. Верный скрипт:
   `$ADM -X PUT -H "$H" -H "$J" -d '{"text":"require [\"fileinto\",\"mailbox\"]; if header :contains \"subject\" \"отчёт\" { fileinto :create \"Отчёты\"; }"}' $API/sieve/scripts/main`,
   затем `$ADM -X POST -H "$H" $API/sieve/scripts/main/activate`; в списке скриптов `main` — `active: true`.
3. Письмо с темой «Недельный отчёт» от `b@example.test` на `a@example.test` (`swaks`, как в CMP-01) — в Postman у `a`
   появилась папка «Отчёты», письмо в ней, во «Входящих» его нет.
4. Письмо с темой «Обед» — во «Входящих».
5. То же через submission (587) от `b` — письмо с темой «отчёт» тоже в «Отчёты».
6. `$ADM -X POST -H "$H" $API/sieve/deactivate` — следующее письмо с темой «отчёт» — во «Входящих».

## Автоответ через Admin API
1. `$ADM -X PUT -H "$H" -H "$J" -d '{"enabled":true,"subject":"Я в отпуске","text_body":"Вернусь в понедельник."}' $API/vacation`;
   `$ADM -H "$H" $API/vacation` — те же значения.
2. Письмо внешнего отправителя (`swaks --from ext@sender.test --to a@example.test`, порт 25, заголовок `To:` с адресом
   `a`) — письмо доставлено; в очереди исходящей почты — автоответ для `ext@sender.test` с темой «Я в отпуске»
   (`$DC logs thepost | grep -i vacation`, либо получен ящиком отправителя, если стенд доставляет наружу).
3. Второе письмо того же отправителя сразу после первого — доставлено, нового автоответа нет (один за период,
   `sieve.vacation.default_days`, по умолчанию 7 суток).
4. Письмо другого внешнего отправителя — свой автоответ.
5. Письмо рассылки (`List-Id: <x.example.org>`) или с `Auto-Submitted: auto-replied` — автоответа нет.
6. `$ADM -X PUT -H "$H" -H "$J" -d '{"enabled":false}' $API/vacation` — письмо третьего отправителя, автоответа нет.

## Автоответ в Postman (JMAP VacationResponse)
1. Postman, учётная запись `a`: «Настройки → Автоответ» показывает состояние из предыдущего раздела (выключен).
2. Включить, задать текст и срок действия (с сегодня по завтра), сохранить; `$ADM -H "$H" $API/vacation` —
   `enabled: true`, тот же текст, `from`/`to`.
3. Письмо нового внешнего отправителя — автоответ с текстом из Postman, один раз.
4. Изменение через Admin API (шаг 1 предыдущего раздела с другим текстом) — в открытом Postman текст обновился без
   перезапуска (push `VacationResponse`).
5. Срок действия в прошлом (`to` — вчера) — автоответа нет.

## Правило и автоответ вместе
1. Скрипт из раздела «Правило» включён, автоответ включён: письмо с темой «отчёт» от нового внешнего отправителя —
   письмо в «Отчёты», автоответ в очереди.

## Конфигурация
1. В config.yaml `sieve: { max_script_size: 64 }`, перезапуск: запись скрипта из раздела «Правило» через Admin API —
   `422` (скрипт больше предела); вернуть значение по умолчанию.
2. Неизвестный ключ в секции `sieve` — `thepost config check` сообщает ключ и секцию.
