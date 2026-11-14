# APP-78: узел — подписанные пакеты признаков: проверка, установка, обновление, откат, офлайн-импорт

lane: app
deps: [META-87, ADM-51, SMTP-52, SIEVE-15, DEPS-19]
class: A
files: [crates/thepost/src/*, crates/thepost/tests/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/CTR-95.md, docs/tasks/ADM-51.md]
checks: [nextest]
manual: no

## Сделать
- Узел получает `RiskStore` (`PgMeta`) и передаёт действующий каталог доставке SMTP и Sieve, а интерфейс пакетов —
  Admin API; кэш каталога сбрасывается по `risk_changed`.
- Пакет — архив ZIP (`zip`, DEPS-19): `manifest.json`, `manifest.sig`, списки и образцы. Проверка: подпись
  Ed25519 (`ring`) ключом из `risk.trusted_keys` по `key_id` (ключи открытые, в конфигурации); хэши SHA-256 всех
  файлов по манифесту; `version` больше активной; `min_node` не выше версии узла. Любая ошибка — `Rejected { report }`,
  активный пакет не меняется.
- Lint: схема манифеста, известные узлу детекторы, компиляция выражений, разбор списков и PSL, источники `Bundled`
  — не из `FORBIDDEN_BUNDLED` и с `commercial: true` (CTR-95). Пробный прогон: детекторы и каталог на письмах
  `samples` пакета — признаки совпадают с `expect`, иначе отказ; `UpdateReport` (добавлено, удалено, изменено,
  осиротевшие изменения администратора) — в отчёт.
- Установка: архив — в BLOB, затем `put_package` и `activate` (БД — без обращений к BLOB внутри транзакции); прошлые
  версии сверх `risk.package.keep` (по умолчанию 5) удаляются из BLOB после фиксации; откат — `rollback(version)` на
  хранимую.
- Обновление — роль maintenance: раз в `risk.update.interval` (по умолчанию сутки) берёт пакет с `risk.update.url`
  (HTTPS, `If-None-Match`) или из каталога `risk.update.dir` (закрытый контур: файл кладут руками или зеркалом);
  после проверки — `activate`, если `risk.update.auto_activate` (по умолчанию да: новые признаки всё равно
  «только отчёт»), иначе ждёт администратора. Нет пакета или сбой — работает прежний, предупреждение в `doctor`.
- Срок «только отчёт»: признак поставщика с `since` старше `risk.report_only_days` (по умолчанию 14) без решения
  администратора — переходит в `Enforce`, событие аудита и строка в отчёте.
- Команды узла: `thepost risk verify <файл>` (подпись, lint, пробный прогон без установки),
  `thepost risk pack --manifest … --key <SecretRef|файл>` — сборка и подпись пакета (для выпуска, DEP-57).
- Первый пакет — из поставки (`/usr/share/thepost/risk/`, DEP-57) при пустом хранилище.

## Как делают другие
- sa-update: подпись GPG обязательна, lint перед установкой, `--install` из файла с `.asc`
  (https://spamassassin.apache.org/full/4.0.x/doc/sa-update.html); Rspamd: `sign+` карты, `trusted_keys`,
  `fallback+file://` (https://docs.rspamd.com/configuration/maps); Stalwart — правила при старте с сохранением
  настроек администратора (https://stalw.art/docs/spamfilter/rules/); зеркало ClamAV для закрытого контура
  (https://docs.clamav.net/appendix/CvdPrivateMirror.html).
- Отличие: номер версии только растёт (откат — только на хранимую), обязательный пробный прогон на корпусе пакета.

## Приёмка
- e2e узла: пакет v2 ставится поверх v1, вес администратора сохранён; v2 повторно и v1 — отказ «номер не растёт»;
  откат на v1 — каталог v1; пакет с изменённым файлом списка — отказ по хэшу; с чужим ключом — отказ по подписи.
- Пакет, где образец не даёт ожидаемый признак, — `Rejected` с отчётом; активный не изменился.
- Закрытый контур: файл в `risk.update.dir` без сети ставится; `thepost risk verify` на нём — тот же отчёт.
