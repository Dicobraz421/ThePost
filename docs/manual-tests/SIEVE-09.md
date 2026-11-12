# SIEVE-09: ManageSieve — команды скриптов — ручная проверка

Проверка возможна после APP-27 (слушатель ManageSieve в узле) и DEP-21 (порт 4190 на стенде). Нужны: эталон
`--profile smb` запущен (`docs/manual-tests/CMP-01.md`, раздел «Запуск»), учётные записи `a@example.test` и
`b@example.test` с паролями приложений (`docs/manual-tests/APP-19.md`), `openssl`, `swaks`, Thunderbird с дополнением
«Sieve» (thsmith/sieve) либо отдельное приложение Sieve Editor.

## Протокол (openssl)
1. `openssl s_client -connect localhost:4190 -starttls sieve -quiet` — после рукопожатия сервер повторяет возможности:
   `"IMPLEMENTATION" "The Post"`, `"SASL" "PLAIN OAUTHBEARER"`, `"SIEVE" "fileinto envelope …"`, `"VERSION" "1.0"`,
   завершающая строка `OK`. Строки `"STARTTLS"` после рукопожатия нет.
2. Вход: `AUTHENTICATE "PLAIN" "<base64 от \0a@example.test\0пароль>"`
   (`printf '\0a@example.test\0пароль' | base64`) — `OK "Authenticated"`. Неверный пароль — `NO "Authentication failed"`,
   третья неудача за сеанс — `BYE` и закрытие.
3. Без TLS (`nc localhost 4190`) в приветствии `"SASL" ""`, на `AUTHENTICATE` — `NO (ENCRYPT-NEEDED)`.
4. `LISTSCRIPTS` — `OK` без строк; `CHECKSCRIPT "fileinto \"Work\";"` — `NO` с текстом `строка 1, столбец 1: …`.
5. `PUTSCRIPT "test" "keep;"` — `OK`; `SETACTIVE "test"`; `LISTSCRIPTS` — `"test" ACTIVE`; `DELETESCRIPT "test"` —
   `NO (ACTIVE)`; `SETACTIVE ""`, `DELETESCRIPT "test"` — `OK`; `LOGOUT` — `OK`, соединение закрыто.

## Thunderbird
1. Учётная запись `a@example.test` (IMAP стенда). В дополнении Sieve: сервер — хост стенда, порт 4190, STARTTLS,
   вход — как в IMAP. Подключение проходит, список скриптов пуст.
2. Создать скрипт `rules`:
   ```
   require "fileinto";
   if header :contains "subject" "отчёт" {
       fileinto "Work";
   }
   ```
   Сохранить — без ошибок. Внести ошибку (убрать `require`) и сохранить — редактор показывает сообщение сервера со
   строкой и столбцом; исходный текст на сервере не изменился (открыть заново).
3. Создать в почтовом клиенте папку `Work`. Активировать `rules` — в списке отметка активного. Удаление активного
   скрипта отклоняется; переименование в `rules2` проходит, активность сохраняется.
4. `swaks --server localhost:587 -tls --auth-user b@example.test --auth-password <пароль> --from b@example.test
   --to a@example.test --header "Subject: отчёт за неделю"` — письмо появляется в папке `Work`, во «Входящих» его нет.
   Письмо с другой темой — во «Входящих».
5. Деактивировать скрипт, повторить письмо с темой «отчёт» — оно во «Входящих». Удалить скрипт — список пуст.
6. Под `b@example.test` список скриптов пуст: скрипты `a@example.test` не видны.

## Лимиты
1. Скрипт больше `max_script_size` хранилища (128 КиБ по умолчанию): редактор сообщает об отказе (`QUOTA/MAXSIZE`).
2. Семнадцатый скрипт (лимит 16): отказ (`QUOTA/MAXSCRIPTS`).
3. Сеанс без команд дольше `sieve.managesieve.idle_timeout` (30 минут) — `BYE "Idle timeout"`, клиент переподключается.
