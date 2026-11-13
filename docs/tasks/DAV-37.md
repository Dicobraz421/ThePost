# DAV-37: GAL: почтовые контакты в книге «Организация»

lane: dav
deps: [META-80, DAV-27]
class: A
files: [crates/tp-dav/src/*, crates/thepost/src/services.rs, crates/thepost/src/gal.rs]
read: [docs/tasks/META-80.md, docs/CONTRACTS.md#Directory]
checks: [nextest]
manual: no

Хвост META-80 (#745): почтовые контакты Admin API видны в `Directory::contacts_since`, но книгу строит
`tp_dav::gal::sync` только из `accounts_since` и `groups_since` (приёмка CMP-10, ошибка 1, шаг 3.2; SRS 4.5).

## Сделать
- `gal::sync` читает и `contacts_since` (порциями, как учётные записи) и строит карточки контактов: `UID`, `FN`,
  `EMAIL`, телефоны из атрибутов; контакт без адреса — без карточки.
- `GalDeps::contacts: Option<Arc<dyn ContactStore>>`: контакт, скрытый из GAL (`MailContact::hidden_from_gal`), — по
  `external_id` или идентификатору контакта — в книгу не попадает (как скрытые группы; для LDAP-каталога признак
  хранится только в `ContactStore`). Узел передаёт `PgMeta` (`Services::gal`).
- Изменение и удаление контакта — как у учётных записей: полная выгрузка каталога при каждой записи книги.

## Приёмка
- Тест: контакт виден в книге, скрытый — нет; изменение — новая версия только его карточки; после удаления из
  каталога карточки нет.
