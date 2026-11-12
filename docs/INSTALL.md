# Установка The Post

Для администратора: как поставить сервер, завести первого администратора и пользователей, подключить почтовые
клиенты. Подробности по каждому компоненту — [`deploy/README.md`](../deploy/README.md).

## Что понадобится

- **Linux** x86_64 или AArch64 с glibc 2.18 и новее. Для рабочего сервера — Debian 13 (основа) или 12, Ubuntu 26.04
  или 24.04, Astra Linux SE 1.8, РЕД ОС 8, AlmaLinux 10 или 9 (совместимость с RHEL 9 и 10), от 4 vCPU и 8 ГБ памяти
  (целевой размер для 1 000 пользователей), диск — под письма.
- **Имя сервера** (например, `mail.example.ru`) и **почтовый домен** (`example.ru`), доступ к их DNS.
- **Открытые порты**: 25 (приём почты), 465 и 587 (отправка), 993 и 143 (IMAP), 4190 (ManageSieve — правила
  фильтрации), 443 (JMAP, автонастройка), 80 (только для выпуска сертификата Let's Encrypt).
- Все требования к окружению и тонкости настройки (кодировка базы UTF-8 и др.) — [PREREQUISITES.md](PREREQUISITES.md).

Все внутренние компоненты ставятся вместе с сервером: PostgreSQL (метаданные), OpenBao (секреты), Rspamd (антиспам).

## Попробовать: Docker Compose

Самый быстрый способ посмотреть — на любой машине с Docker Engine и Compose v2, от 4 ГБ памяти. Домен в этом
варианте тестовый — `example.test`, сертификаты выпускает свой тестовый центр.

```bash
git clone https://github.com/Dicobraz421/ThePost && cd ThePost
docker compose -f deploy/compose/dev.yml --profile smb up -d --build
```

Первая сборка образа идёт 20–30 минут (Rust собирается внутри Docker, на машине его ставить не нужно). Без сборки —
готовый образ выпуска из ghcr.io. Образ закрытый: сначала войдите в реестр токеном GitHub с правом `read:packages`
(выдаёт владелец репозитория), `<версия>` — из выпуска (тег образа совпадает с `appVersion` чарта Helm):

```bash
docker login ghcr.io -u <логин GitHub>          # пароль — токен read:packages
THEPOST_IMAGE=ghcr.io/dicobraz421/thepost:<версия> docker compose -f deploy/compose/dev.yml --profile smb up -d
```

Когда все контейнеры запущены, проверьте окружение:

```bash
docker compose -f deploy/compose/dev.yml --profile smb run --rm --no-deps thepost doctor
```

В итоге не должно быть строк `FAIL`. Предупреждения `WARN` про DKIM и DNS для тестового домена — ожидаемы.

Дальше — [первый администратор](#первый-администратор). В этом варианте команды узла запускаются через
`docker compose -f deploy/compose/dev.yml --profile smb exec thepost thepost …`, а CA для клиентов —
`deploy/compose/tls/ca.pem`.

Остановить — `… --profile smb down`; данные остаются в томах Docker и вернутся при следующем `up`.

## Рабочий сервер: Ansible

Чистая ВМ (Debian 13 или 12, Ubuntu 26.04 или 24.04, Astra Linux SE 1.8, РЕД ОС 8, AlmaLinux 10 или 9) →
PostgreSQL 16, OpenBao, Rspamd (по желанию — антивирус ClamAV) и `thepost`
под systemd, одной командой с управляющей машины. Повторный запуск ничего не ломает — им же применяются изменения.

1. Скачайте выпуск и распакуйте бинарный файл:

   ```bash
   git clone https://github.com/Dicobraz421/ThePost && cd ThePost
   V=beta-20261111.2   # последний выпуск — на странице Releases
   curl -LO https://github.com/Dicobraz421/ThePost/releases/download/$V/thepost-$V-x86_64-unknown-linux-gnu.tar.gz
   tar xzf thepost-$V-x86_64-unknown-linux-gnu.tar.gz
   ```

2. Опишите сервер:

   ```bash
   cp deploy/ansible/inventory.example.yml inventory.yml
   ```

   В `inventory.yml` — адрес ВМ и пользователь SSH, `thepost_domain` (почтовый домен), `thepost_hostname`
   (имя сервера), `thepost_tls_mode` и `thepost_acme_email`:

   | `thepost_tls_mode` | Сертификат |
   |---|---|
   | `acme` (по умолчанию) | Let's Encrypt встроенным клиентом The Post; порт 80 открыт, DNS имени сервера указывает на ВМ; ключ — в OpenBao |
   | `files` | свои файлы PEM: `thepost_tls_cert_src`, `thepost_tls_key_src` |
   | `internal` | внутренний центр сертификации — только для стенда |

   Закрытый ключ сертификата хранится только в OpenBao (секрет `thepost/tls`), на диске сервера его нет: плейбук
   кладёт его туда сам, при `acme` — сам узел. Сменить свой сертификат (`files`) — заменить файлы и повторить прогон.

3. Запустите:

   ```bash
   ansible-galaxy collection install -r deploy/ansible/requirements.yml
   ansible-playbook -i inventory.yml deploy/ansible/smb.yml \
     -e thepost_binary_src=$PWD/thepost-$V-x86_64-unknown-linux-gnu/thepost
   ```

   Плейбук сам дождётся готовности узла. Ключи восстановления OpenBao он кладёт в `/root/openbao-init.json` на ВМ —
   **перенесите их в надёжное место и удалите с сервера.**

4. Проверьте на ВМ: `sudo -u thepost thepost doctor` — без `FAIL`.

## Вручную под systemd

Если PostgreSQL и OpenBao уже есть или нужна своя схема — установка бинарного файла, юнита systemd и `config.yaml`
по шагам описана в [`deploy/README.md`](../deploy/README.md#установка-под-systemd-smb-enterprise-вм). Секреты (пароли
СУБД, ключи, в том числе ключ TLS — `tls.key: { secret: "thepost/tls#key" }`) в `config.yaml` не пишутся — только
ссылки на OpenBao.

## Первый администратор

На сервере, от root или от учётной записи службы:

```bash
sudo thepost admin init --email admin@example.ru
```

Команда заводит администратора и принятый домен и один раз выводит:

- **пароль** администратора — им можно войти в почту по IMAP, SMTP и JMAP;
- **одноразовый токен** Admin API (`tpa_…`), действует 10 минут.

Сохраните оба значения сразу: повторно они не показываются. Повторный запуск откажет (`409`); заново —
`thepost admin init --force`, при этом прежний пароль `admin init` перестанет действовать.

## Учётные записи

Пользователи заводятся командой `thepost admin` на самом сервере: Admin API слушает только локальный адрес
(`127.0.0.1:8443`), адрес команда берёт из `config.yaml`. Сначала обменяйте одноразовый токен на сессию (8 часов):

```bash
THEPOST_ADMIN_TOKEN=tpa_… thepost admin login --insecure     # выводит сессию tps_…
export THEPOST_ADMIN_TOKEN=tps_…
```

`--insecure` — только для этого локального адреса, если сертификат узла выпущен на имя сервера (иначе — `--ca-file`).
Дальше — пользователь и пароль для его почтового клиента:

```bash
thepost admin account create ivanov@example.ru --name "Иван Иванов" --insecure
thepost admin app-password create ivanov@example.ru --label Thunderbird --insecure   # пароль — один раз
thepost admin account get ivanov@example.ru --insecure
thepost admin domain list --insecure
```

У каждого клиента может быть свой пароль приложения: `app-password list` показывает их без значений,
`app-password revoke` отзывает один, не трогая остальные. Вывод — таблица или `--json`.

В Docker Compose те же команды выполняются в контейнере узла:

```bash
DC="docker compose -f deploy/compose/dev.yml --profile smb"
$DC exec -e THEPOST_ADMIN_TOKEN=tpa_… thepost thepost admin login --insecure
$DC exec -e THEPOST_ADMIN_TOKEN=tps_… thepost thepost admin account create a@example.test --insecure
```

## Настройка клиентов

| | Сервер | Порт | Защита |
|---|---|---|---|
| Входящая почта (IMAP) | `mail.example.ru` | 993 | SSL/TLS |
| | | 143 | STARTTLS |
| Исходящая почта (SMTP) | `mail.example.ru` | 465 | SSL/TLS |
| | | 587 | STARTTLS |
| Правила фильтрации (ManageSieve) | `mail.example.ru` | 4190 | STARTTLS |
| JMAP | `https://mail.example.ru/.well-known/jmap` | 443 | HTTPS |

Имя входа — адрес целиком (`ivanov@example.ru`), пароль — пароль приложения. Подходит любой клиент IMAP и SMTP
(например, Thunderbird), правила фильтрации — клиент ManageSieve (дополнение Thunderbird Sieve, `sieve-connect`),
по JMAP — [Postman](https://github.com/Dicobraz421/Postman). В тестовом варианте с `example.test` клиент попросит принять
сертификат: его выпустил тестовый центр (`deploy/compose/tls/ca.pem`).

## DNS домена

Чтобы почта приходила и не попадала в спам у получателей:

| Запись | Значение |
|---|---|
| `MX` домена | `10 mail.example.ru.` |
| `A` / `AAAA` сервера | адрес ВМ; обратная запись (PTR) адреса — `mail.example.ru` |
| `TXT` домена (SPF) | `v=spf1 mx -all` |
| `TXT _dmarc` | `v=DMARC1; p=quarantine; rua=mailto:postmaster@example.ru` |
| `TXT <селектор>._domainkey` (DKIM) | открытый ключ подписи, см. ниже |

**Подпись DKIM.** Закрытый ключ хранится в OpenBao, в `config.yaml` — только ссылка на него. Каким селектором подписывать, задаёт политика организации или домена (Admin API, поле `dkim_selector`); без неё письмо подписывается всеми ключами домена:

```yaml
smtp:
  dkim:
    domains:
      example.ru:
        keys:
          s2026: { algorithm: rsa-sha256, key: { secret: "thepost/dkim#example.ru" } }
```

`thepost doctor` проверяет MX, SPF, DKIM, DMARC, MTA-STS и PTR по публичному DNS и подсказывает, чего не хватает.

## Kubernetes: Helm

Чарт — `deploy/helm/thepost`, до него — OpenBao и секрет TLS (порядок — в
[`deploy/README.md`](../deploy/README.md#kubernetes-helm)). Образ в ghcr.io закрытый: нужен секрет реестра с токеном
GitHub (`read:packages`), без `imagePullSecrets` чарт не устанавливается и подсказывает, что сделать:

```bash
kubectl -n thepost create secret docker-registry ghcr --docker-server=ghcr.io   --docker-username=<логин GitHub> --docker-password=<токен read:packages>
helm install thepost deploy/helm/thepost -n thepost --set 'imagePullSecrets[0].name=ghcr' -f <свои значения>
```

Тег образа по умолчанию — `appVersion` чарта: установка закреплена за выпуском. Другой тег — `--set image.tag=…`.

## Обновление

- **Ansible** — скачайте новый выпуск и повторите `ansible-playbook … -e thepost_binary_src=…`: узел перезапустится,
  миграции базы применятся сами.
- **Docker Compose** — `git pull` и `… --profile smb up -d --build` (с готовым образом — новая `<версия>` в
  `THEPOST_IMAGE` и `… up -d`).
- **Helm** — `git pull` и `helm upgrade thepost deploy/helm/thepost -n thepost --reuse-values`: тег образа — новый
  `appVersion` чарта.

Перед обновлением рабочего сервера сделайте внеочередную копию: `sudo systemctl start thepost-backup@full` (см. ниже).

## Резервное копирование и восстановление

Плейбук `smb.yml` сам включает резервное копирование (`thepost_backup: true`, отключить — `false`) готовыми
средствами по таймерам systemd:

- **база** (PostgreSQL) — [pgBackRest](https://pgbackrest.org/): журнал изменений (WAL) уходит в копию непрерывно,
  не реже раза в минуту; полная копия — в ночь на воскресенье, разностная — в остальные ночи. Восстановить можно
  состояние на любой момент за последние `thepost_backup_retention_days` суток (по умолчанию 7);
- **каталог писем** (`/var/lib/thepost/blob`, тела писем) — [restic](https://restic.net/) (по умолчанию: снимки,
  шифрование) или rsync (`thepost_backup_blob_tool: rsync`, зеркало): каждый час и после каждой копии базы.

Где лежат копии:

| Параметр инвентаря | По умолчанию | Что это |
|---|---|---|
| `thepost_backup_repo` | `posix` | `posix` — каталог на сервере, `s3` — объектное хранилище S3 (только HTTPS) |
| `thepost_backup_repo_path` | `/var/lib/pgbackrest` | каталог копий базы (или префикс в бакете S3) |
| `thepost_backup_s3_endpoint`, `thepost_backup_s3_bucket` | — | адрес S3 (`https://…`) и бакет |
| `thepost_backup_restic_repo` | `/var/backups/thepost/restic` | копии писем restic: каталог или `s3:https://<адрес>/<бакет>/<префикс>` |
| `thepost_backup_rsync_dest` | `/var/backups/thepost/blob-mirror` | зеркало rsync: каталог или `user@host:каталог` (SSH) |
| `thepost_backup_blob_schedule` | `hourly` | как часто копировать письма (пусто — только после копий базы) |

**Копия на том же диске не спасёт при отказе диска**: подключите к этим каталогам отдельный диск или сетевой том,
или храните копии в S3.

**Ключи.** Копии базы шифруются (AES-256), репозиторий restic — тоже. Ключ шифрования и пароль restic плейбук
создаёт в OpenBao при первом запуске (`thepost-backup/pgbackrest`, `thepost-backup/restic`; узлу The Post они
недоступны), на сервере они лежат только зашифрованными (`systemd-creds`). Ключи S3 положите в OpenBao сами — на
сервере, с токеном администратора OpenBao (root-токен — в `/root/openbao-init.json`, пока вы его не перенесли):

```bash
export BAO_ADDR=https://localhost:8200 BAO_CACERT=/etc/openbao/tls/ca.pem BAO_TOKEN=…
bao kv put -mount=thepost-backup s3 access_key_id=… secret_access_key=…
```

**Сохраните ключ шифрования и пароль restic вне сервера** (рядом с ключами восстановления OpenBao): если сервер
потерян целиком, без них копии не прочитать.

```bash
bao kv get -mount=thepost-backup -field=cipher_pass pgbackrest
bao kv get -mount=thepost-backup -field=password restic
```

Проверка на сервере (от root):

```bash
systemctl list-timers 'thepost-backup*'      # расписание
thepost-pgbackrest info                      # копии базы и архив WAL
thepost-restic snapshots                     # снимки каталога писем
journalctl -u 'thepost-backup*' --since today
systemctl start thepost-backup@full          # внеочередная полная копия (затем — копия писем)
```

### Восстановление

С управляющей машины — на момент `T` (время с часовым поясом; без `thepost_restore_target` — последнее состояние):

```bash
ansible-playbook -i inventory.yml deploy/ansible/restore.yml -e thepost_restore_confirm=true \
  -e "thepost_restore_target='2026-11-11 18:00:00+03'"
```

Плейбук останавливает узел и таймеры копий, возвращает базу на момент `T`, кладёт каталог писем из первой копии,
сделанной после `T` (без `T` — из последней; другой снимок — `-e thepost_restore_blob_snapshot=<ID>`), поверх текущего, выставляет пароли учётных записей СУБД из OpenBao, запускает узел и проверяет `thepost verify`:
если у какого-то письма нет тела, плейбук остановится с ошибкой. Лишние тела (письма, пришедшие после `T`) удалит
`thepost verify --fix` в том же прогоне, если им больше суток, иначе — позже сам узел (поиск «сирот»). В конце —
новая полная копия.

Почему копия писем, сделанная после `T`, подходит: тело письма удаляется с диска не раньше окна W после удаления
последнего письма, которое на него ссылается (`blob.gc.window`, по умолчанию — глубина восстановления + 8 суток;
SRS 3.5). Поэтому потерять можно только письма, пришедшие после последней копии каталога писем (по умолчанию —
за последний час), и только при потере самого диска: `thepost verify` покажет их как повреждённые. Пустой или
пропавший каталог писем таймер не копирует. При потере диска указывайте `T` — момент перед потерей.

**Новый сервер вместо потерянного:**

1. Установите The Post без копирования: `ansible-playbook … smb.yml -e thepost_backup=false`.
2. Сделайте копии доступными по тем же путям (подключите диск с копиями) или укажите тот же S3.
3. Положите сохранённые ключи в OpenBao нового сервера:

   ```bash
   bao secrets enable -path=thepost-backup -version=2 kv
   bao kv put -mount=thepost-backup pgbackrest cipher_pass=…
   bao kv put -mount=thepost-backup restic password=…
   # S3 — как выше: bao kv put -mount=thepost-backup s3 …
   ```

4. Запустите `restore.yml`, как выше: он сам поставит средства копирования, восстановит базу и письма и включит
   таймеры; дальше `smb.yml` — как обычно.

### Новый сервер вместо потерянного

Копия OpenBao (секреты узла, в том числе ключи DKIM) снимается вместе с копией базы — снимок Raft в то же
хранилище копий. Заранее храните **вне сервера**: `/root/openbao-init.json`, `/etc/openbao/unseal.key` и пароль restic.

1. Установите новый сервер без копирования и без ожидания готовности узла:
   `ansible-playbook … deploy/ansible/smb.yml -e thepost_backup=false -e thepost_wait_ready=false`. Прогон завершится
   успешно с предупреждением «thepost не готов»: узел ждёт секретов прежнего сервера (ключ DKIM и другие), они
   вернутся из снимка OpenBao.
2. Верните копии по тем же путям и выполните `chown -R postgres:postgres /var/lib/pgbackrest`.
3. В новом OpenBao заведите только хранилище пароля копий:
   `bao secrets enable -path=thepost-backup -version=2 kv` и
   `bao kv put -mount=thepost-backup restic password=…` (для S3 — и его ключи). Ключ шифрования pgBackRest вернётся из снимка.
4. Восстановите: `ansible-playbook … deploy/ansible/restore.yml -e thepost_restore_confirm=true
   -e thepost_restore_openbao=true -e thepost_restore_openbao_init_src=<openbao-init.json>
   -e thepost_restore_openbao_static_key_src=<unseal.key>` (при распечатывании ключами Шамира —
   `thepost_restore_openbao_unseal_keys`). Снимок OpenBao ставится до восстановления базы.
5. Обычный прогон `smb.yml` — узел стартует, DKIM подписывает тем же ключом, `thepost verify` — без потерь; ключ TLS
   из снимка прогон сводит с сертификатом сервера (`internal` — новый сертификат к ключу, `files` и `certbot` — свой
   ключ новой версией секрета).

## Проверка подписи

Архивы, SBOM и образ подписаны [cosign](https://docs.sigstore.dev/) без ключей; рядом с каждым файлом лежит
`<файл>.sigstore.json`. Готовая команда проверки — в описании каждого выпуска, например:

```bash
f=thepost-$V-x86_64-unknown-linux-gnu.tar.gz
cosign verify-blob "$f" --bundle "$f.sigstore.json" \
  --certificate-identity 'https://github.com/Dicobraz421/ThePost/.github/workflows/beta.yml@refs/heads/main' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Если что-то не так

- `thepost doctor` — первая проверка: что именно не так и как исправить.
- Журнал узла — `journalctl -u thepost` (в Docker — `docker compose … logs thepost`); пароли и токены в него не пишутся.
- Вопросы и ошибки — в [issues](https://github.com/Dicobraz421/ThePost/issues).
