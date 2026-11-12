# CMP-01: эталон `--profile smb` принимает соединения — ручная проверка

Нужны: чистая машина с Docker (Compose v2), свободные порты 25, 465, 587, 143, 993, 443 (или переопределение
`TP_*_PORT`, см. `deploy/README.md`), `openssl`, `curl`; для письма — `swaks` и клиент IMAP или JMAP (Postman).

## Запуск
1. `docker compose -f deploy/compose/dev.yml --profile smb up -d --build` — `tls-init`, `openbao-init-tls`,
   `postgres-role-tls` завершаются с кодом 0, `thepost` работает (`docker compose … ps`).
2. `docker compose -f deploy/compose/dev.yml --profile smb logs thepost` — нет `слушатели роли не заданы`;
   нет ошибок чтения `/etc/thepost/tls/fullchain.pem`, секрета `thepost/tls#key` (ключ TLS — только в OpenBao) и
   `/etc/ssl/certs`.
3. `docker compose -f deploy/compose/dev.yml --profile smb run --rm --no-deps thepost doctor` — `tls.certificate`,
   `tls.names` (`mail.example.test`), `database.*`, `blob.*` — `OK`. Известные `FAIL` на 2026-11-10: `dns.mx`
   (у `example.test` нет MX — ожидаемо для тестового домена) и `database.node_role` (нет роли `thepost_app` — META-17).

## Порты (CA — `deploy/compose/tls/ca.pem`, его копирует `tls-init`)
1. `openssl s_client -connect localhost:465 -CAfile deploy/compose/tls/ca.pem -verify_return_error </dev/null` —
   `Verify return code: 0 (ok)`, баннер `220 mail.example.test ESMTP The Post`, в `EHLO` — `AUTH PLAIN LOGIN …`.
2. `openssl s_client -connect localhost:587 -starttls smtp -CAfile deploy/compose/tls/ca.pem </dev/null` и то же
   для `:25` — STARTTLS проходит, `Verify return code: 0 (ok)`.
3. `openssl s_client -connect localhost:993 -CAfile deploy/compose/tls/ca.pem </dev/null` — `* OK [CAPABILITY … AUTH=PLAIN …]`;
   `openssl s_client -connect localhost:143 -starttls imap -CAfile deploy/compose/tls/ca.pem </dev/null` — STARTTLS проходит.
4. `curl --cacert deploy/compose/tls/ca.pem -si https://localhost/.well-known/jmap` — `HTTP/1.1 401 Unauthorized`
   (`www-authenticate: Basic`, `Bearer`), не обрыв соединения.
5. Сертификат проверяется и по имени `mail.example.test`: `curl --cacert … --resolve mail.example.test:443:127.0.0.1
   https://mail.example.test/.well-known/jmap` — без ошибки TLS.
6. Без учётных записей: `RCPT TO:<b@example.test>` на 25 — `550 5.1.1 Mailbox unavailable`,
   `RCPT TO:<z@other.test>` — `550 5.7.1 Relaying denied`.

## Письмо между двумя пользователями (после CMP-04, CMP-05 и решения issue #193)
1. Завести `a@example.test` и `b@example.test` (способ — по решению issue #193 «первая учётная запись SMB»).
2. `swaks --server localhost:587 --tls --tls-ca-path deploy/compose/tls/ca.pem --auth PLAIN --auth-user a@example.test
   --auth-password … --from a@example.test --to b@example.test` — `250` на `DATA`.
3. Входящий без AUTH: `swaks --server localhost:25 --from x@remote.test --to b@example.test` — `250`;
   на `--to nobody@other.test` — отказ (`550`/`554`, не открытый релей).
4. IMAP (993) под `b@example.test`: оба письма в `INBOX`; JMAP `Email/query` по `b@example.test` — те же письма.
5. У `a@example.test` после входа есть Sent (CMP-05); письмо из п. 2 доставлено сразу, без очереди smtp-out (CMP-04).

## Повторный запуск
1. `docker compose … --profile smb down` и снова `up -d` — сертификаты не перевыпускаются (`tls: сертификаты уже есть`),
   `openbao-init-tls` ключ TLS не перезаписывает (нет строки `записан ключ TLS`), в томе `tls` нет `privkey.pem`;
   узел поднимается так же.
