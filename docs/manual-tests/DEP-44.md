# DEP-44: ключ TLS слушателей — только в OpenBao (эталоны) — ручная проверка

Нужно: ВМ эталона SMB (`smb.yml`), узел с `tls.key: { secret }` (PLT-24, сборка `main` или выпуск после него).

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `smb.yml` с `thepost_tls_mode: internal` | `failed=0`; `Create key client (in memory)` и `Write secret thepost/tls` — `changed`; узел готов (`/healthz/ready`) |
| 2 | `find / -xdev -name privkey.pem -o -name client.key`; `ls /etc/thepost/tls` | пусто; в `/etc/thepost/tls` — только `fullchain.pem` |
| 3 | `grep -A2 '^tls' /etc/thepost/config.yaml` | `cert_file` и `key: { secret: thepost/tls#key }`, `key_file` нет |
| 4 | Открытый ключ секрета `bao kv get -mount=thepost -field=key tls \| openssl pkey -pubout` и сертификата `openssl x509 -in /etc/thepost/tls/fullchain.pem -noout -pubkey` | совпадают |
| 5 | `openssl s_client -connect 127.0.0.1:993 -servername mail.example.test -CAfile /etc/thepost/pki/ca.pem -verify_return_error` | `subject=CN=mail.example.test`, `Verify return code: 0 (ok)` |
| 6 | Повторить шаг 1 | `changed=0`; версия секрета `thepost/tls` прежняя |
| 7 | `smb.yml -e thepost_tls_mode=files -e thepost_tls_cert_src=… -e thepost_tls_key_src=…`; затем ещё раз | первый — новая версия секрета, цепочка заменена, узел отдаёт новый сертификат без перезапуска; второй — `changed=0` |
| 8 | `bao policy read thepost` | `thepost/data/tls` — `read`, `create`, `update`; остальное `thepost/data/*` — только чтение |
| 9 | compose: `docker compose -f deploy/compose/dev.yml up tls-init openbao-tls openbao-init-tls` дважды | первый — `записан ключ TLS thepost/tls`, второй — без неё; `sha256` секрета и `server.key` совпадают; `privkey.pem` в томе `tls` нет |
| 10 | `helm template deploy/helm/thepost`; `thepost config check` для `config.yaml` из ConfigMap и пресета `deploy/config/all.yaml` (с переменными `THEPOST_*` из `compose/dev.yml`) | `tls.key: { secret: thepost/tls#key }`; том `tls` — только `tls.crt` и `ca.crt`; «конфигурация верна» |

## Прогон 2026-11-12

Стенд: school-99, контейнер Debian 13 с systemd (`--privileged`, `mount --make-rshared /`, данные OpenBao — tmpfs),
Ansible в отдельном контейнере (`community.docker`); узел — сборка CI `main` `0e1898a` (выпуск `beta-20261112`
собран до PLT-24 и `tls.key` не понимает). Особенности стенда (на ВМ не нужны): процессор без AVX2 — узел под
`qemu-user`, в нём нет seccomp — `thepost_config_extra: { confine: { seccomp: "off", landlock: false } }`.

| # | Итог |
|---|---|
| 1 | `failed=0`; ключ выпущен в памяти, секрет `thepost/tls` версии 1; узел готов |
| 2 | файлов ключа узла нет; `/etc/thepost/pki` — только `ca.key` и `internal.key` (службы PostgreSQL и OpenBao) |
| 3 | `key: { secret: thepost/tls#key }` |
| 4 | совпадают |
| 5 | `CN=mail.example.test`, `Verify return code: 0 (ok)` |
| 6 | `changed=0` |
| 7 | первый — `changed=2` (цепочка, секрет версии 2), узел без перезапуска (`NRestarts=0`) отдаёт новый сертификат; второй — `changed=0` |
| 8 | как ожидается |
| 9 | как ожидается (`current_version 1` после второго запуска) |
| 10 | как ожидается |
