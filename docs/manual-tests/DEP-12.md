# DEP-12: стенд SMB — учётные записи через thepost admin и наполнение по Д.3 — ручная проверка

Стенд — как в [DEP-04](DEP-04.md): узел SMB и генератор, установленные `deploy/stand/stand.yml --skip-tags run`.
Узел SMB — выпуск с CLI `thepost admin account` / `app-password` (APP-21; beta-20261111.3 и новее).

## Что делает
- `--tags provision` (`stand_provision: admin`, по умолчанию): на узле SMB `thepost admin init` один раз (пароль
  администратора — в состоянии стенда; утрачен — `init --force`), вход по паролю `admin login --password-stdin`
  (истёкшая сессия — снова вход по паролю), затем `account create` и `app-password create` для
  `user1 … user<N>`, зондов `probe1 … probe3` и ящика `big`. Состояние — `/var/lib/thepost-stand-admin/` на узле
  (0700, файлы 0600), пароли — `/etc/thepost-stand/accounts` на генераторе (0600, `login:пароль`). В вывод Ansible
  попадают только счётчики (`created=… existing=… issued=… skipped=…`), задачи с паролями — `no_log`.
- `--tags fill`: `fill.py` — письма корпуса на 25 (`stand_fill_parallel` сеансов, не быстрее `stand_fill_rate`/с),
  `stand_fill_messages` на ящик нагрузки и `stand_big_messages` малых писем в ящик `big`; принятое — в
  `/var/lib/thepost-stand/fill-state.json` (повтор досылает только недостающее); сверка — IMAP STATUS выборки
  `stand_fill_verify` ящиков (0 — все).
- `stand_provision: none | ldif` — по-прежнему: общий пароль `stand_user_password`, файл `accounts` из него.

## Шаги
| # | Шаг | Ожидание |
|---|---|---|
| 1 | `ansible-playbook -i inventory.yml deploy/stand/stand.yml --tags provision,fill -e stand_users=50 -e stand_fill_messages=100 -e stand_fill_verify=0` | `failed=0`; `created=54 … issued=54`; `fill: … accepted total=15000 of 15000`; `visible=15000 of accepted=15000` |
| 2 | На генераторе: `stat -c '%a %U' /etc/thepost-stand/accounts`; `wc -l` того же файла | `600 root`; 54 строки |
| 3 | На узле: `stat -c '%a' /var/lib/thepost-stand-admin{,/accounts,/session}` | `700`, `600`, `600` |
| 4 | В выводе шага 1 и журнале Ansible найти пароль любой строки `accounts` (`grep -c`) | 0 |
| 5 | Вход IMAP каждой учётной записи: `python3 /var/lib/thepost-stand/probes.py count --accounts /etc/thepost-stand/accounts --host mail.stand.test --domain stand.test --users 50 --ca /etc/thepost-stand/ca.pem --out-json /tmp/c.json` | без ошибок входа; `per` — 54 записи, у `user<n>` ≥ 100, у `big` ≥ 10 000 |
| 6 | Повтор шага 1 | `created=0 existing=0 issued=0 skipped=54`; `fill: planned=0 delivered=0`; задачи `changed=0` |

## Итог уменьшенного прогона (2026-11-11)
Стенд — контейнеры Docker на `school-99-stand` (Debian 12 с systemd: узел SMB и генератор, 4 vCPU / 4 GB на всех),
узел — beta-20261111.3, `stand_users=50`, `stand_fill_messages=100`, `stand_big_messages=10000`. Ansible — в
контейнере, подключение `community.docker.docker_api`.

| Шаг | Факт |
|---|---|
| 1 (первый запуск) | `failed=0`; `created=53 existing=0 issued=53 skipped=0` (до добавления `probe3`); `fill: planned=15000 delivered=15000 failed=0 retries=26 in 1052s` (≈ 14 писем/с; повторы — `451 4.3.0 Temporary failure` при занятом пуле СУБД 3); `visible=11900 of accepted=11900` (выборка 20 ящиков); `big: 10000 messages` |
| 1 (после добавления `probe3`) | `created=1 existing=0 issued=1 skipped=53 total=54` |
| 2, 3 | `600 root`; на узле `700` / `600` / `600` |
| 4 | пароль в журналах Ansible — 0 вхождений |
| 5, 6 | повтор `--tags provision,fill -e stand_fill_verify=0`: `created=0 existing=0 issued=0 skipped=53`, `fill: planned=0 delivered=0`, `visible=15000 of accepted=15000 (mailboxes=51)` — вход IMAP всех учётных записей, 5 000 писем ящиков нагрузки и 10 000 в `big` видны; `changed=0` |

Найдено в эталоне SMB (чужая полоса, на стенде обойдено локально): роль `postgresql` передаёт
`postgresql_user` параметр `update_password` (такого нет ни в 3.x, ни в 4.x community.postgresql), а в 4.x убран
и `port` (requirements.yml допускает ≥ 3.4); роль `thepost` пишет `{{ ansible_managed }}` в `copy: content`
(переменная не определена); роль `openbao` — `set -o pipefail` с `… | grep -q` (SIGPIPE: повторный прогон
падает на `bao auth enable approle`, «path is already in use»). Особенности контейнера (не узла): `LoadCredential`
systemd 252 в Docker не выдаётся — секрет подложен через `stand_config_extra`; `cluster.node_id` задан явно.

## Что модель не покрывает (на сегодня)
- Наполнение — через SMTP 25 (фильтры Milter, очередь), не Admin API импорта (п. 5.2: метода импорта нет) и без
  корпуса Enron; текст — синтетический словарь корпуса стенда.
- Квота стенда Д.5 (`quota_bytes: 50 MB`) меньше наполнения Д.3 (1 500 × 75 KB ≈ 110 MB на ящик): полное
  наполнение упрётся в квоту — решение владельца (отчёт волны deploy-12). Ящик `big` наполняется малыми письмами
  (≈ 1 KB), чтобы 10 000 писем уложились в квоту.
- Повтор после сбоя посреди доставки может дослать до ~1 с принятых писем повторно (отметка сохраняется раз в 1 с).
