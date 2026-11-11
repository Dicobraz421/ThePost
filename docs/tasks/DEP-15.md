# DEP-15: резервное копирование SMB — pgBackRest и копия каталога писем

lane: deploy
deps: [DEP-05]
class: A
files: [deploy/ansible/roles/pgbackrest/*, deploy/ansible/roles/thepost/*, deploy/ansible/smb.yml, deploy/ansible/inventory.example.yml, deploy/README.md, docs/INSTALL.md, docs/manual-tests/DEP-15.md]
read: [docs/spec/APP-Z.md, docs/spec/S5.md#5.3, docs/spec/S3.md#3.5]
checks: [none]
manual: yes

## Сделать
- Роль `pgbackrest` (пакет дистрибутива, готовое средство): архив WAL и полные/разностные копии по расписанию
  (systemd timer), хранилище — локальный каталог или S3 (`thepost_backup_repo`); ключи S3 и шифрования — из OpenBao,
  не в файлах инвентаря.
- Копия каталога BLOB по расписанию после копии СУБД (готовое средство — `restic` или `rsync` по выбору в
  `thepost_backup_blob_tool`), порядок соблюдает инвариант W (п. 3.5): копия BLOB не старше окна W относительно PITR.
- `smb.yml`: роль включается `thepost_backup: true` (по умолчанию — да для рабочего сервера).
- Восстановление: плейбук `deploy/ansible/restore.yml` — PITR на время `T`, возврат каталога BLOB, `thepost verify`
  (потерянных объектов — 0); раздел «Резервное копирование и восстановление» в `docs/INSTALL.md`.

## Приёмка
- На чистой ВМ: установка с копированием, письма, копия, удаление данных, восстановление на `T`, `thepost verify`
  без потерь, письма на месте — `docs/manual-tests/DEP-15.md`.
