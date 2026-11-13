# DEP-53: развёртывание: KRB5RCACHEDIR — каталог кэша повторов Kerberos узла

lane: deploy
deps: [SYS-16]
class: A
files: [deploy/ansible/roles/thepost/tasks/main.yml, deploy/helm/thepost/templates/deployment.yaml, deploy/README.md]
read: [docs/tasks/SYS-16.md, docs/tasks/DEP-39.md]
checks: [none]
manual: yes

## Сделать
- Хвост SYS-16 (#722): Landlock при `auth.gssapi` разрешает кэш повторов Kerberos только в `/var/lib/thepost/krb5rcache`
  (`tp_sys::KRB5_RCACHE_DIR`), а MIT Kerberos по умолчанию пишет в `/var/tmp` — каталог задаёт развёртывание.
- Ansible: drop-in `gssapi.conf` — после `Environment=KRB5_KTNAME=…` строка `Environment=KRB5RCACHEDIR=/var/lib/thepost/krb5rcache`.
- Helm: при `gssapi.keytabSecret` рядом с `KRB5_KTNAME` — `KRB5RCACHEDIR` (каталог в томе `state`).
- Образ: `KRB5_KTNAME` в нём не задаётся — правок нет. `deploy/README.md`: ручной drop-in и Helm.
- Каталог создаёт сам узел до Landlock (APP-68).

## Приёмка
- Ручная проверка на стенде с KDC: `systemctl show thepost -p Environment` содержит `KRB5RCACHEDIR`, вход
  `AUTHENTICATE GSSAPI` в IMAP проходит, кэш — в `/var/lib/thepost/krb5rcache`, отказов Landlock в журнале нет.
- Источник: хвост SYS-16 (#722).
