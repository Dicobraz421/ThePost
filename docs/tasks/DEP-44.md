# DEP-44: эталоны: ключи TLS в OpenBao

lane: deploy
deps: []
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/PLT-24.md]
checks: [none]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 8; решение 100.

## Сделать
- Роль `thepost_pki` и `tls-files`: ключи слушателей и внутренних каналов кладутся в OpenBao, в config.yaml —
  `key: { secret: … }`; compose, Helm, `deploy/config/*.yaml` — так же; политика `deploy/openbao/thepost-policy.hcl` —
  запись ключа ACME в свой путь (PLT-29); `deploy/README.md`.

## Приёмка
- Стенд: узел стартует без файлов ключей на диске; повторный прогон без изменений.
