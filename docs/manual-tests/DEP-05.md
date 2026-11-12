# DEP-05: эталонная установка SMB одной командой — ручная проверка

Нужно: чистая ВМ Debian 12 или Ubuntu 24.04 (4 vCPU, 8 GB, доступ в интернет), управляющая машина с Ansible ≥ 2.15.
Для `thepost_tls_mode: acme` (по умолчанию) — DNS-имя ВМ и открытый порт 80; без них — `thepost_tls_mode: internal`.

| # | Шаг | Ожидание |
|---|---|---|
| 1 | `cargo build --release`; `ansible-galaxy collection install -r deploy/ansible/requirements.yml` | бинарный файл `target/release/thepost`, коллекции установлены |
| 2 | Инвентарь из `deploy/ansible/inventory.example.yml` (адрес ВМ, `thepost_domain`, `thepost_hostname`, способ сертификата) | — |
| 3 | `ansible-playbook -i inventory.yml deploy/ansible/smb.yml` | `failed=0`; последняя задача `Wait for thepost readiness` — `ok` |
| 4 | На ВМ: `systemctl is-active postgresql openbao rspamd thepost` | все `active` |
| 5 | На ВМ: `journalctl -u thepost -b -p warning` | нет ошибок; `curl -sk https://127.0.0.1:8081/healthz/ready` — 200 |
| 6 | На ВМ: `ss -ltnp` | 25, 143, 443, 465, 587, 993 — `thepost` на всех адресах; 5432, 8200, 11332–11334 — только loopback |
| 7 | С управляющей машины: `openssl s_client -connect <ВМ>:993 -servername <thepost_hostname>` | сертификат на `thepost_hostname` (acme — Let's Encrypt; internal — CA `/etc/thepost/pki/ca.pem`) |
| 8 | `swaks --server <ВМ> --to postmaster@<домен> --data 'Subject: test\n\ntest'` | `250` (или `550` неизвестному адресату — учётные записи заводит issue #193); в журнале `thepost` — проверка фильтром `rspamd` |
| 9 | На ВМ: `psql "postgres://postgres@localhost/thepost?sslmode=disable"` от root | `no pg_hba.conf entry` / `reject` — открытый канал отклоняется |
| 10 | Повторить шаг 3 | `changed=0` |
| 11 | С `thepost_clamav: true`, шаг 3; письмо с тестовой строкой EICAR | `systemctl is-active clamav-milter` — `active`; письмо EICAR отклонено (`OnInfected Reject`) |
| 12 | Перезагрузить ВМ | после загрузки `thepost` активен без вмешательства (OpenBao распечатан автоматически) |
