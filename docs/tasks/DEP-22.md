# DEP-22: сертификаты узлов с URI thepost:node:<id>

lane: deploy
deps: [CLU-14]
class: A
files: [deploy/ansible/roles/thepost_pki/*, deploy/helm/thepost/*, deploy/README.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

## Сделать
- `thepost_pki` и Helm (cert-manager) выпускают сертификат узла с SAN URI `thepost:node:<cluster.node_id>` плюс DNS
  (решение 32); раздел о собственном УЦ заказчика в `deploy/README.md`.

## Приёмка
- `openssl x509 -noout -ext subjectAltName` на выпущенном сертификате показывает URI и DNS.
