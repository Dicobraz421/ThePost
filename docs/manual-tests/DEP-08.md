# DEP-08: минимальные привилегии в эталонной поставке — ручная проверка

Команды — из корня репозитория. ВМ — Debian 12 / Ubuntu 24.04 с systemd ≥ 252; compose — Docker Engine, Compose v2.

## systemd: юнит и сокеты (без запуска)
1. `systemd-analyze verify deploy/systemd/thepost.service deploy/systemd/thepost-*.socket` → замечаний нет
   (без установленного бинарного файла — только «/usr/local/bin/thepost is not executable»).
2. Юнит отдельно (каталог `deploy/systemd/thepost.service.d` рядом systemd-analyze подхватывает как drop-in):
   `T=$(mktemp -d); cp deploy/systemd/thepost.service $T/; systemd-analyze security --offline=true $T/thepost.service | tail -1`
   → `Overall exposure level … 1.2 OK` (не хуже 2.0).

## systemd: установка на ВМ (Ansible, эталон SMB)
1. `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` → прогон без ошибок, `/healthz/ready` — 200; повторный
   прогон — `changed=0`.
2. `systemd-analyze security thepost.service | tail -1` → не хуже 2.0 (ожидается 1.2).
   `systemctl show thepost -p CapabilityBoundingSet -p AmbientCapabilities` → пусто (ни одной capability, решение 102).
   `systemctl is-active thepost-{smtp,submissions,submission,imap,imaps,https}.socket` → `active`;
   `ss -ltnp 'sport < 1024'` → порты 25, 143, 443, 465, 587, 993 слушает `systemd` (pid 1) и передаёт `thepost`.
3. Привилегии процесса службы — doctor в окружении службы (те же учётная запись, capabilities, NoNewPrivileges):
   ```bash
   sudo systemd-run --wait --pipe --quiet -p User=thepost -p Group=thepost -p NoNewPrivileges=yes \
     -p CapabilityBoundingSet= -p AmbientCapabilities= \
     -p LoadCredential=openbao-secret-id:/etc/thepost/credentials/openbao-secret-id \
     /usr/local/bin/thepost doctor --config /etc/thepost/config.yaml | grep privileges
   ```
   → `privileges.user`, `privileges.capabilities`, `privileges.no_new_privs`, `privileges.seccomp`, `privileges.landlock` — `OK`
   (на ядре без Landlock — `WARN unsupported`, это свойство ядра); `capabilities … eff=- bnd=-`.
4. `ss -ltnp | grep 8443` → `127.0.0.1:8443` (Admin API только на loopback); `curl -sk https://<адрес ВМ>:8443/` — отказ.
5. PostgreSQL:
   ```bash
   sudo -u postgres psql -d thepost -Atc "select rolname, rolcanlogin, rolsuper, rolbypassrls from pg_roles where rolname like 'thepost%' order by 1" \
     -c "select distinct tableowner from pg_tables where schemaname = 'public'" \
     -c "select pg_has_role('thepost', 'thepost_app', 'MEMBER'), pg_has_role('thepost_readonly', 'thepost_reader', 'MEMBER')"
   ```
   → `thepost|t|f|f`, `thepost_app|f|f|f`, `thepost_owner|t|f|f`, `thepost_reader|f|f|f`, `thepost_readonly|t|f|f`;
   владелец таблиц — `thepost_owner`; `t|t`. `thepost doctor` → `database.node_role OK: роль = thepost, владелец
   схемы = thepost_owner`.
6. Обновление установки до DEP-08 (таблицы принадлежат `thepost`): прогон плейбука → таблицы переданы `thepost_owner`,
   узел стартует, `database.node_role` — `OK`.
7. Сокеты systemd: `openssl s_client -connect <ВМ>:465` → `220`; `systemctl restart thepost` во время `nc <ВМ> 25` в
   цикле — соединения не отклоняются (ждут в очереди сокета).

## Compose (профиль smb, образ уже собран — `--no-build`)
1. `docker compose -f deploy/compose/dev.yml --profile smb up -d --no-build` → `postgres-role-tls`: «БД thepost — владелец
   thepost_owner, узел thepost (thepost_app), только чтение thepost_readonly»; `openbao-init-tls`: записаны
   `thepost/db`, `thepost/db-owner`, `thepost/db-readonly`; `thepost` — running, в журнале
   «роль принимает соединения role=admin addrs=[127.0.0.1:8443]».
2. `docker compose -f deploy/compose/dev.yml --profile smb run --rm --no-deps thepost doctor | grep -E 'privileges|node_role|rls'`
   → `privileges.user uid=65532`, `privileges.capabilities eff=- bnd=-`, `no_new_privs 1`, `seccomp enforce` — `OK`;
   `privileges.landlock` — `OK abi=<N>` (проба `landlock_create_ruleset`, PLT-21; Debian 13 — `abi=6`; `WARN abi=<N>` —
   ядро старше 6.10, ограничения частичные; `WARN unsupported` — Landlock выключен в ядре или вызов запрещён профилем
   seccomp Docker); `database.node_role`, `database.rls` — `OK`.
3. `docker inspect thepost-thepost-1 --format '{{.HostConfig.ReadonlyRootfs}} {{.HostConfig.CapDrop}} {{.HostConfig.SecurityOpt}}'`
   → `true [ALL] [no-new-privileges:true]`.
4. Узлу недоступен пароль только чтения: вход AppRole (как шаг 3 `DEP-07.md`) и `bao kv get -mount=thepost db-readonly`
   → `permission denied`; `db-owner` — читается.

## Helm (не проверено без кластера)
1. `helm lint deploy/helm/thepost` и `helm template t deploy/helm/thepost | kubeconform -strict` → без ошибок.
2. В манифесте пода: `runAsNonRoot: true`, `readOnlyRootFilesystem: true`, `allowPrivilegeEscalation: false`,
   `capabilities.drop: [ALL]`, `seccompProfile.type: RuntimeDefault`; порты контейнера выше 1024, Service — 25…993, 443.
3. В кластере: под Ready (пробы `/healthz/*` на 8081), `kubectl exec` недоступен (distroless) —
   `kubectl port-forward deploy/t 8443` → Admin API отвечает на `https://127.0.0.1:8443`.
