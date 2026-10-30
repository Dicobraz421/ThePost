# DEP-07: OpenBao в эталонной поставке, роль приложения без суперпользователя — ручная проверка

Linux или WSL2 с Docker Engine / Docker Desktop, Compose v2. Команды — из корня репозитория.
`C=deploy/compose/dev.yml`.

## Профиль tls (по умолчанию)
1. `docker compose -f $C up -d --wait` → `openbao-tls` healthy; `openbao-init-tls`, `postgres-role-tls` — `Exited (0)`.
   `docker compose -f $C logs openbao-init-tls` → «хранилище инициализировано», «выдан secret_id», «записан секрет thepost/db».
2. `curl -s --cacert deploy/compose/tls/ca.pem https://localhost:8200/v1/sys/seal-status` → `"type":"static"`,
   `"sealed":false`; `curl -s http://localhost:8200/` — ошибка (только TLS).
3. Вход AppRole и чтение секрета (из контейнера с томами стенда):
   ```bash
   docker compose -f $C run --rm --no-deps --entrypoint sh -v thepost_openbao-approle:/a:ro openbao-init-tls -c \
     'export BAO_TOKEN=$(bao write -field=token auth/approle/login role_id=$(cat /a/role_id) secret_id=$(cat /a/secret_id));
      bao token lookup | grep policies; bao kv get -mount=thepost -field=username db; bao kv put -mount=thepost db x=1'
   ```
   → политики `[default thepost]`, `thepost`; запись — `permission denied`.
4. Роль PostgreSQL: пароль из OpenBao, подключение по TLS:
   ```bash
   PW=$(docker compose -f $C run --rm --no-deps --entrypoint sh openbao-init-tls -c \
     'BAO_TOKEN=$(awk "/Initial Root Token/ {print \$NF}" /admin/init.txt) bao kv get -mount=thepost -field=password db')
   PGPASSWORD=$PW psql "postgres://thepost@localhost:5432/thepost?sslmode=verify-full&sslrootcert=deploy/compose/tls/ca.pem" \
     -c "select rolsuper, rolbypassrls, rolcreaterole from pg_roles where rolname = current_user" -c "create table t(x int); drop table t"
   ```
   → `f | f | f`; таблица создаётся (роль — владелец БД `thepost`). С неверным паролем — `password authentication failed`.
5. Перезапуск: `docker compose -f $C restart openbao-tls` → через несколько секунд снова healthy, `"sealed":false` без ввода ключей.
6. Повторный `docker compose -f $C up -d --wait` → `openbao-init-tls`: только «готово»; `postgres-role-tls`: «нового пароля нет».
7. Старый том `tls` без SAN `openbao-tls` (стенд DEP-06): `up` → `tls-init`: «перевыпуск серверного сертификата», CA прежний.

## Профиль dev
1. `docker compose -f $C --profile dev up -d --wait` → `openbao` healthy, `openbao-init` и `postgres-role` — `Exited (0)`.
2. `curl -s -H 'X-Vault-Token: thepost-dev-root' http://localhost:8200/v1/thepost/data/db` → `username thepost`, `password thepost-dev`.
3. `curl -s -X POST -d '{"role_id":"thepost-dev","secret_id":"thepost-dev-secret-id"}' http://localhost:8200/v1/auth/approle/login`
   → `token_policies: [default, thepost]`.
4. `PGPASSWORD=thepost-dev psql -h localhost -U thepost -d thepost -c 'select rolsuper, rolbypassrls from pg_roles where rolname = current_user'` → `f | f`.
5. `docker compose -f $C --profile dev down -v`.

## Профиль smb и doctor (после PLT-12, META-17)
1. `docker compose -f $C --profile smb up -d --build` → `thepost` стартует после `postgres-role-tls` и healthy `openbao-tls`.
2. `docker compose -f $C --profile smb run --rm thepost doctor` → без `Fail`: роль без SUPERUSER/BYPASSRLS (`Ok`),
   хранилище секретов доступно, вход AppRole (`Ok`), секретов в config.yaml нет (`Ok`); ключ распечатывания в файле — `Warn`.
3. `docker compose -f $C --profile smb down -v`.

## Ansible (ВМ Debian 12 или контейнер с systemd)
1. `ansible-playbook -i <инвентарь> deploy/ansible/openbao.yml -e openbao_tls_cert_src=… -e openbao_tls_key_src=… -e openbao_tls_ca_src=…`
   → без ошибок; `/root/openbao-init.json` (0400), `/etc/openbao/unseal.key` (0400 openbao),
   `/etc/thepost/openbao/role_id`, `/etc/thepost/credentials/openbao-secret-id` (0600 root); предупреждение о ключе в файле.
2. Повторный прогон → `changed=0`.
3. `systemctl restart openbao; bao status` → `Seal Type static`, `Sealed false`.

## Helm
1. `helm template openbao openbao/openbao --version 0.30.6 -n openbao -f deploy/helm/openbao/values.yaml` → StatefulSet
   на 3 реплики, `BAO_CACERT`, тома `openbao-tls`, `openbao-unseal`, ClusterRoleBinding `system:auth-delegator`.
2. В кластере: установка, `bao operator init`, `bootstrap.sh` → под с сервисным аккаунтом `thepost` в пространстве
   `thepost` входит `bao write auth/kubernetes/login role=thepost jwt=@/var/run/secrets/kubernetes.io/serviceaccount/token`
   и читает `thepost/db`.
