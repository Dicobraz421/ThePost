# DEP-09: эталоны HA СУБД (pg_auto_failover, CloudNativePG) — ручная проверка

Команды — из корня репозитория. Приёмка: переключение primary на стенде — RTO ≤ 30 с, узлы thepost продолжают работу.
Нагрузка во время переключения — сценарий стенда (`deploy/stand`, Д.6) или цикл отправки писем через 587.

## Без стенда
1. YAML: `python3 -c 'import sys,yaml; [yaml.safe_load_all(open(f)) for f in sys.argv[1:]]' deploy/ansible/pg-ha.yml
   deploy/ansible/inventory-ha.example.yml deploy/config/mailbox.yaml` → без ошибок.
2. `ansible-playbook -i deploy/ansible/inventory-ha.example.yml deploy/ansible/pg-ha.yml --syntax-check` → без ошибок.
3. `helm template t deploy/helm/thepost --set cnpg.enabled=true | kubectl apply --dry-run=server -f -` (кластер с
   оператором CloudNativePG 1.26+) → Cluster `t-db`, Pooler `t-db-pooler-rw`; в ConfigMap `database.url` — Pooler,
   `pgbouncer: true`, `direct_url` — `t-db-rw`. С `--set cnpg.pooler.enabled=false` — `url` — `t-db-rw`, без `pgbouncer`.
4. `helm template t deploy/helm/thepost` (по умолчанию) → ресурсов CloudNativePG нет, `database.url` — из values.

## Enterprise-ВМ: pg_auto_failover
1. Инвентарь по `deploy/ansible/inventory-ha.example.yml` (монитор, pg1–pg3, сертификаты с SAN — имя узла), пароли в
   vault. `ansible-playbook -i inventory-ha.yml deploy/ansible/pg-ha.yml --ask-vault-pass` → без ошибок; повторный
   прогон — `changed=0`, primary не перезапускался.
2. На мониторе: `sudo -u postgres /usr/lib/postgresql/16/bin/pg_autoctl show state --pgdata /var/lib/postgresql/16/pgaf`
   → один `primary`, два `secondary`, все `read-write`/`read-only` и `yes` в «Healthy».
   `... pg_autoctl show settings` → `number_sync_standbys 1`, `synchronous_standby_names` — `ANY 1 (…)`.
3. Без интерпретаторов: `systemctl show pgautofailover -p ExecStart` → `/usr/lib/postgresql/16/bin/pg_autoctl run`;
   `ps -o comm= --ppid $(systemctl show -p MainPID --value pgautofailover)` → только `postgres`, `pg_autoctl`;
   `pg_lsclusters` → кластеров Debian нет.
4. TLS: `psql "host=pg1.example.ru dbname=thepost user=thepost sslmode=disable"` → отказ (`pg_hba` reject);
   с `sslmode=verify-full sslrootcert=ca.pem` → вход.
5. Узлы thepost: `thepost_db_url` — pg1, `thepost_db_urls: [pg2, pg3]` (или `deploy/config/mailbox.yaml`) →
   `thepost config check`, `thepost doctor` без `Fail`; в `/metrics` — `tp_db_failover_primary 0`.
6. Переключение (отказ primary): под нагрузкой на primary `sudo systemctl kill -s SIGKILL pgautofailover` или
   выключение ВМ. Замер — от отказа до `tp_db_failover_switching 0` на узлах thepost:
   `tp_db_failover_last_switch_seconds` ≤ 30, `tp_db_failover_switches_total` +1, `tp_db_failover_primary` — номер
   нового primary; `pg_autoctl show state` — новый `primary`, прежний — после возврата `secondary`.
   Во время переключения — временные отказы (SMTP 4xx), после — отправка и IMAP/JMAP работают, писем не потеряно
   (число принятых 250 = число доставленных).
7. Изоляция прежнего primary (без второго primary): отключить сеть primary (`iptables -A INPUT -j DROP` кроме
   управления) → через ≈ 10 с прежний primary — только чтение (`SHOW transaction_read_only` → `on`), новый primary —
   на другом узле не раньше, чем через `primary_demote_timeout` (15 с).
8. Плановое переключение: `pg_autoctl perform switchover --pgdata …` на мониторе → узлы thepost продолжают работу.

## Enterprise-K8s: CloudNativePG
1. Оператор (`helm install cnpg cnpg/cloudnative-pg …`), секреты `thepost-db-owner`, `thepost-db`,
   `thepost-db-readonly`, `helm install thepost deploy/helm/thepost --set cnpg.enabled=true …` →
   `kubectl cnpg status thepost-db` — 3 экземпляра, синхронная реплика `quorum`/`sync`; поды thepost — Ready.
2. `kubectl exec thepost-db-1 -- ps -eo comm` → только `postgres`, `manager` (без python, perl).
3. Переключение: `kubectl delete pod <primary> --grace-period=0 --force` под нагрузкой → новый primary,
   `tp_db_failover_last_switch_seconds` ≤ 30 (или время недоступности службы `thepost-db-rw`), узлы thepost
   продолжают работу, писем не потеряно.
4. Плановое: `kubectl cnpg promote thepost-db thepost-db-2` → то же.

## SMB: PostgreSQL без pg_ctlcluster
1. `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` поверх существующей установки → PostgreSQL перезапущен
   один раз, повторный прогон — `changed=0`.
2. `systemctl show postgresql@16-main -p ExecStart` → `/usr/lib/postgresql/16/bin/postgres -D …`; `systemctl reload
   postgresql` перечитывает настройки; `thepost` — `/healthz/ready` 200.
