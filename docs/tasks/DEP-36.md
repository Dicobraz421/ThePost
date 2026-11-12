# DEP-36: эталон: движок PKI OpenBao для внутренних сертификатов

lane: deploy
deps: [PLT-26]
class: A
files: [deploy/ansible/roles/openbao/*, deploy/ansible/roles/thepost_pki/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Часть PLT-26 в полосе deploy (решение 64), выделена по решению 71 (карточка — одна полоса).

## Сделать
- Роль `openbao`: движок PKI (корневой и промежуточный УЦ, роль выпуска с коротким сроком), политика узла на выпуск
  своих сертификатов core-компонентов (узлы — URI `thepost:node:<id>`, решение 32; СУБД, OpenBao, SeaweedFS).
- Роль `thepost_pki`: при включённом PKI OpenBao — файловый УЦ не создаётся, узлы получают сертификаты через PLT-26;
  без него — как сейчас.

## Приёмка
- `ansible-lint`, `--syntax-check`; на стенде — узел получает сертификат из PKI OpenBao и продлевает его без перезапуска.
