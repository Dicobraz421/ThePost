# DEP-04: стенд SMB и базовая нагрузка — ручная проверка

Стенд (APP-D Д.2): узел SMB 4 vCPU / 8 GB / 200 GB NVMe и генератор 8 vCPU / 16 GB / 100 GB, Debian 12 или
Ubuntu 24.04, общая сеть. Управляющая машина: Ansible ≥ 2.15, `cargo build --release`.

## Подготовка
1. `ansible-galaxy collection install -r deploy/ansible/requirements.yml`.
2. Инвентарь из `deploy/stand/inventory.example.yml`: адреса `ansible_host`, `stand_users` (Д.3: 1 000),
   `stand_user_password` — в ansible-vault.
3. Учётные записи `user1@stand.test` … `user<N>@stand.test` с паролем `stand_user_password` заводятся до прогона:
   каталог (LDAP / FreeIPA, секция `auth.ldap` — `stand_config_extra`; LDIF для импорта — `stand_provision: ldif`,
   файл `/etc/thepost-stand/users.ldif` на генераторе) или штатный способ issue #193, когда появится.

## Шаги
| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/stand/stand.yml --skip-tags run` | `failed=0`; на SMB — эталон DEP-05 с `thepost_tls_mode: internal` |
| 2 | На SMB: `grep -A3 retention /etc/thepost/config.yaml`; `grep relay /etc/thepost/config.yaml` | пороги Д.5 (`trash_retention: 15m`, `quota_bytes: 52428800`, …); `relay: <генератор>:2525` |
| 3 | На генераторе: `k6 version`; `podman image ls`; `systemctl is-active thepost-stand-sink`; `ls /var/lib/thepost-stand/corpus \| wc -l` | k6 1.0.0; образ imaptest; `active`; 202 файла (200 писем, mbox, словарь) |
| 4 | Короткий прогон: `ansible-playbook -i inventory.yml deploy/stand/stand.yml --tags run -e stand_duration=300` | `failed=0`; итог в выводе; каталог `deploy/stand/reports/<прогон>-base/` |
| 5 | Открыть `summary.txt` прогона | частоты по Д.3 (1 000 пользователей: входящих ≈ 4,17/с, исходящих ≈ 1,04/с; IMAP-клиентов 600, push 300); `smtp-in fail` и `submission fail` — 0 или единицы; `smtp-sink received` ≈ половине `submission ok`; коды выхода инструментов — 0 |
| 6 | `k6-summary.json`, `imaptest.log` | k6: `http_req_failed` < 1 %; imaptest — без `Error:` и разрывов соединений |
| 7 | `smb-metrics-before.txt` / `smb-metrics-after.txt`, `smb-journal-warnings.txt` | счётчики писем выросли; в журнале нет ошибок (предупреждения порогов стенда допустимы) |
| 8 | Через 15–20 мин после прогона: удалить письмо в корзину (IMAP/JMAP) и проверить её | письмо окончательно удалено по `trash_retention: 15m` (Д.5) |
| 9 | Базовый сценарий одной командой: `ansible-playbook -i inventory.yml deploy/stand/stand.yml` | установка без изменений (`changed` — только задачи прогона), прогон 2 ч, отчёт собран |
| 10 | `-e stand_scenario=peak` | множитель 2, 1 ч; частоты в `summary.txt` вдвое выше |

## Что модель не покрывает (на сегодня)
- Календарные операции Д.3 (15 в сутки, 3 приглашения): CalDAV в `thepost serve` не подключён.
- Rate limit «соединений с IP» Д.5: параметра нет (только `limits.max_connections` на слушатель).
- Наполнение Д.3 (1 000 ящиков × 1 500 писем, Enron) через Admin API импорта: такого метода Admin API пока нет;
  ящики наполняются входящим потоком прогона.
- Набор команд imaptest — `stand_imaptest_args` (вероятности команд imaptest, приближение профиля клиентов).
