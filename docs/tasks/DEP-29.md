# DEP-29: ручная проверка: резервные копии SMB в S3

lane: deploy
deps: [DEP-15]
class: A
files: [deploy/ansible/roles/pgbackrest/*, docs/manual-tests/DEP-15.md]
read: [docs/manual-tests/DEP-15.md]
checks: [none]
manual: yes

Хвост волны deploy-13 (DEP-15), PR #395: вариант хранения копий в S3 (`thepost_backup_repo: s3`) на стенде не проверялся —
прогон был с каталогом.

## Сделать
- Прогон `docs/manual-tests/DEP-15.md` с хранилищем S3 (SeaweedFS стенда или внешний S3, HTTPS, свой CA —
  `thepost_backup_s3_ca_file`): копии pgBackRest и restic в S3, восстановление на `T` и на новой ВМ; находки — правки
  роли `pgbackrest` в этой же карточке.

## Приёмка
- Раздел «Прогон S3» в `docs/manual-tests/DEP-15.md` с фактическими итогами, без «не прошёл».
