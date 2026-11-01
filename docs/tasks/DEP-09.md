# DEP-09: стенды без Valkey, эталоны HA СУБД

lane: deploy
deps: [CLU-09]
class: A
files: [deploy/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S3.md#3.1, docs/spec/S7.md#7.1, docs/spec/APP-D.md#Д.2]
checks: [nextest]
manual: docs/manual-tests/DEP-09.md

## Сделать
- Compose dev/smb: убрать Valkey.
- Эталон Enterprise-ВМ: PostgreSQL с pg_auto_failover (роль Ansible), список адресов primary в config.yaml; Enterprise-K8s: CloudNativePG (Helm).
- Без интерпретаторов в работе стенда (п. 7.1).

## Приёмка
- Переключение primary на стенде: RTO ≤ 30 с, узлы продолжают работу.
