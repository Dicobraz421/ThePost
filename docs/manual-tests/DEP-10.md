# DEP-10: HPA, VPA и эталон «Малый Enterprise» — ручная проверка

Команды — из корня репозитория. Приёмка: `helm template` и проверка схем манифестов; отказ одной ВМ эталона
«Малый Enterprise» на стенде — работа продолжается (приложение З.2).

## Helm: без кластера
1. `helm lint deploy/helm/thepost` и `helm lint deploy/helm/thepost --set autoscaling.enabled=true
   --set resources.requests.cpu=1 --set vpa.enabled=true` → `0 chart(s) failed`.
2. `helm template t deploy/helm/thepost` (по умолчанию) → Deployment с `replicas: 1` и `podAntiAffinity`
   (`preferredDuringScheduling…`, `topologyKey: kubernetes.io/hostname`); HPA, PodDisruptionBudget, VPA нет.
3. `helm template t deploy/helm/thepost --set autoscaling.enabled=true --set resources.requests.cpu=1` →
   - в Deployment нет `replicas` (число подов задаёт HPA);
   - HPA `autoscaling/v2`: `minReplicas: 2`, `maxReplicas: 10`, метрика `cpu` `Utilization` 70, метрик памяти нет;
     `behavior.scaleDown.stabilizationWindowSeconds: 1800`, сжатие — по одному поду за 600 с;
   - PodDisruptionBudget `policy/v1`: `maxUnavailable: 1`, `unhealthyPodEvictionPolicy: AlwaysAllow`.
4. Без `resources.requests.cpu`: `helm template t deploy/helm/thepost --set autoscaling.enabled=true` → ошибка
   «HPA по CPU считает загрузку от resources.requests.cpu».
5. По соединениям: `... --set autoscaling.connections.enabled=true --set metrics.podMonitor.enabled=true` → в HPA
   вторая метрика `type: Pods`, `tp_load_connections`, `averageValue: "1000"`; PodMonitor со `scheme: https` и CA из
   секрета `thepost-tls` (`ca.crt`). Только по соединениям: `--set autoscaling.cpu.targetUtilization=0
   --set autoscaling.connections.enabled=true` → метрика только `Pods`, `resources.requests.cpu` не требуется.
6. `--set vpa.enabled=true` → VerticalPodAutoscaler `autoscaling.k8s.io/v1`, `updateMode: "Off"`,
   `controlledResources: [cpu, memory]`, `controlledValues: RequestsAndLimits`.
7. `--set replicaCount=1` (без HPA) → PodDisruptionBudget нет; `--set replicaCount=2` → есть.
   `--set podAntiAffinity=hard` → `requiredDuringScheduling…`; `--set podAntiAffinity=x` → ошибка.
8. Схемы манифестов — kubeconform (схемы Kubernetes; CRD VPA, Prometheus Operator, CloudNativePG — из CRDs-catalog):
   ```sh
   helm template t deploy/helm/thepost --set autoscaling.enabled=true --set resources.requests.cpu=1 \
     --set autoscaling.connections.enabled=true --set metrics.podMonitor.enabled=true --set vpa.enabled=true \
     --set cnpg.enabled=true \
   | kubeconform -strict -summary -kubernetes-version 1.31.0 -schema-location default \
     -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
   ```
   → `Invalid: 0, Errors: 0`.

## Helm: в кластере (по желанию — kind/k3d, metrics-server, VPA recommender, prometheus-adapter)
1. `helm install t deploy/helm/thepost -n thepost --set autoscaling.enabled=true --set resources.requests.cpu=500m
   --set vpa.enabled=true -f <свои значения>` → `kubectl get hpa,pdb,vpa -n thepost` — объекты есть; поды — на
   разных узлах (`kubectl get pod -o wide`).
2. Нагрузка на CPU (стенд, Д.6) → число подов растёт до `maxReplicas`; после снятия нагрузки — уменьшается не раньше
   чем через 30 мин и по одному поду за 10 мин (`kubectl describe hpa`: события `SuccessfulRescale`).
3. `kubectl drain <узел>` → поды переносятся по одному (PodDisruptionBudget), обслуживание не прерывается.
4. `kubectl describe vpa t` → в `Recommendation` — `Target`, `Lower Bound`, `Upper Bound`; поды не пересоздаются.
5. По соединениям: prometheus-adapter со значениями `deploy/helm/prometheus-adapter/values-thepost.yaml` →
   `kubectl get --raw "/apis/custom.metrics.k8s.io/v1beta1/namespaces/thepost/pods/*/tp_load_connections"` — значения
   подов; рост соединений (imaptest) выше `targetAverageValue` на под → HPA добавляет поды.

## Ansible «Малый Enterprise»: без стенда
1. YAML: `python3 -c 'import sys,yaml; [list(yaml.safe_load_all(open(f))) for f in sys.argv[1:]]'
   deploy/ansible/small-enterprise.yml deploy/ansible/inventory-small.example.yml` → без ошибок.
2. `ansible-galaxy collection install -r deploy/ansible/requirements.yml`;
   `ansible-playbook -i deploy/ansible/inventory-small.example.yml deploy/ansible/small-enterprise.yml --syntax-check`
   → без ошибок. `ansible-lint deploy/ansible/small-enterprise.yml deploy/ansible/roles/seaweedfs` → без ошибок.
3. Эталон SMB не изменился: `ansible-playbook -i deploy/ansible/inventory.example.yml deploy/ansible/smb.yml
   --syntax-check` → без ошибок (роль openbao по умолчанию — один узел, как раньше).

## Ansible «Малый Enterprise»: стенд (5 ВМ Debian 12)
Инвентарь по `deploy/ansible/inventory-small.example.yml`: data1–data3, mail1–mail2, сертификаты в `pki/`
(serverAuth и clientAuth; SAN — имя узла, у ВМ данных — и имя OpenBao, у mailbox — и `localhost`), DNS-имя OpenBao —
A-записи трёх ВМ данных, сертификат клиентских протоколов `mail.example.ru` → виртуальный адрес.

1. `ansible-playbook -i inventory-small.yml deploy/ansible/small-enterprise.yml` → без ошибок; повторный прогон —
   `changed=0`.
2. OpenBao: на data1 `BAO_ADDR=https://data1.example.ru:8200 BAO_CACERT=/etc/openbao/tls/ca.pem bao operator raft
   list-peers` (токен — из `/root/openbao-init.json`) → три узла, один `leader`. `bao kv get -mount=infra seaweedfs`
   — есть; с токеном AppRole thepost → `permission denied` (KV infra узлу недоступен).
3. PostgreSQL: на data3 `sudo -u postgres /usr/lib/postgresql/16/bin/pg_autoctl show state --pgdata
   /var/lib/postgresql/16/pgaf` → `primary` и `secondary`; `pg_autoctl show settings` → `number_sync_standbys 0`
   (db_durability: preferred, приложение З.2).
4. SeaweedFS: на data1 из `/etc/seaweedfs` `echo cluster.ps | weed shell -master=data1.example.ru:9333
   -filer=data1.example.ru:8888` → три master, три volume, три filer; `echo s3.bucket.list | weed shell …` →
   `thepost-blob`. На primary `psql -d thepost -c '\dt seaweedfs.*'` → таблица `filemeta` (метаданные filer).
   Без TLS: `curl http://data1.example.ru:8888/` → отказ (только HTTPS); gRPC без клиентского сертификата → отказ.
5. Узлы mailbox: `thepost config check`, `thepost doctor` без `Fail`; письмо через 587 на виртуальный адрес →
   доставлено, тело — в SeaweedFS (`echo 'fs.ls /buckets/thepost-blob' | weed shell …` — объекты есть; `volume.list`
   — у томов `replication: 001`, копии на двух ВМ).
6. Отказ одной ВМ — под нагрузкой imaptest и smtp-source на виртуальный адрес, по очереди:
   - mail1 (держит виртуальный адрес) — выключить ВМ → адрес на mail2 за ≈ 3 с (`ip addr`), клиенты
     переподключаются ко второму узлу, писем не потеряно (число ответов 250 = число доставленных);
   - data1 (primary PostgreSQL, master, volume, filer, OpenBao) — выключить ВМ → переключение PostgreSQL ≤ 30 с
     (`tp_db_failover_last_switch_seconds`), узлы находят новый primary, на время переключения SMTP отвечает 4xx;
     чтение и запись BLOB продолжаются (кворум master 2 из 3, копия на другой ВМ), OpenBao работает (кворум 2 из 3);
   - data3 (монитор) — выключить ВМ → работа продолжается, автоматическое переключение недоступно до возврата
     монитора;
   - после возврата ВМ — `pg_autoctl show state`: прежний primary — `secondary`; `echo volume.fix.replication |
     weed shell …` — копии восстановлены; `bao operator raft list-peers` — три узла.
7. `thepost verify` после возврата всех ВМ — расхождений нет.
8. Одновременный отказ двух ВМ данных → недоступность (ожидаемо, приложение З.2); после возврата — работа без ручных
   действий, кроме `volume.fix.replication`.
