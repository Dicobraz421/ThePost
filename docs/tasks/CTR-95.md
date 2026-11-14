# CTR-95: контракт каталога признаков и источников TI — признаки, уровни, пакет поставщика, источники

lane: contracts
deps: [CTR-94]
class: B
files: [crates/tp-contracts/src/risk.rs, crates/tp-contracts/src/sieve.rs, crates/tp-contracts/src/events.rs, crates/tp-contracts/src/lib.rs, docs/CONTRACTS.md, crates/tp-testkit/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#126, docs/research/2026-11-14-anomaly-ti.md, docs/tasks/CTR-93.md, docs/tasks/CTR-94.md]
checks: [nextest]
manual: no

## Сделать
- Модуль `risk.rs`. `SignalId` — `[a-z0-9._-]`, до 64 символов; префикс `tp.` — только у признаков поставщика,
  `org.` — признаки администратора. `Origin::{Vendor, Admin}`.
- `Detector` (serde `snake_case`, неизвестный вид — ошибка разбора с именем вида): `thread_new_attachment_type`,
  `encrypted_archive`, `disk_image_or_shortcut`, `thread_new_link_domain`, `reply_to_changed`,
  `list_match { list: ListId, field: Field }`, `Field::{SenderIp, LinkDomain, LinkUrl, FromDomain, AttachmentSha256}`;
  `Detector::drops_reply()` — истинно у пяти признаков беседы (решение 126 г).
- `RiskAction::{None, Tag, Junk, Quarantine, Reject}` (`Ord`, от мягкого к строгому), `Mode::{Report, Enforce}`.
  `Signal { id, detector, weight: u16, threshold: u8, action, mode, origin, since }` (`threshold` — минимальная сила
  срабатывания детектора 0–100; `since` — дата появления, для срока «только отчёт»). `Thresholds { tag, junk,
  quarantine, reject }` — балл, с которого действует действие (по умолчанию 1, 6, 10, 15 — константы `Default`).
- Изменения администратора — `SignalOverride { weight, action, mode }` (поля `Option`) поверх признака поставщика и
  полные признаки `Origin::Admin`. Уровни `RiskLevel::{Installation, Org(TenantId)}`: `RiskPolicy::validate(base)` —
  у организации вес не ниже, действие не мягче, `Report` вместо `Enforce` — `Error::Invalid` с перечнем
  `{signal, field, value, base}`; у установки — любые значения, вес не выше `MAX_WEIGHT`.
- `Catalog::effective(vendor, installation, org) -> EffectiveCatalog` — по признаку: значение и `source` уровня;
  `Catalog::apply_update(old, new_vendor) -> UpdateReport { added, removed, changed, orphaned_overrides }` — заменяет
  только `Origin::Vendor`, изменения администратора сохраняет (модель Stalwart); новые признаки — `Mode::Report`.
- `Verdict { score, hits: Vec<Hit>, report_only: Vec<Hit>, action, drops_reply }`; `Hit { signal, strength, weight,
  source: Detector|Sieve }`; `Verdict::combine(rspamd_action)` — самое строгое.
- Пакет: `PackageManifest { format: 1, version: u64, created, min_node: semver, key_id, signals, lists:
  Vec<ListFile>, samples: Vec<SampleFile> }`, `ListFile { id, path, sha256, entries, kind: Ip|Domain|Url|Sha256,
  source: SourceId }`, `SampleFile { path, sha256, expect: Vec<SignalId> }`. Подпись — отдельные байты Ed25519 над
  каноническим JSON манифеста; проверку делает узел (APP-78), контракт описывает формат. `PackageState::{Active,
  Previous, Rejected { report }}`.
- Источники TI: `TiSource { id: SourceId, tier: Bundled|CustomerKey|Import, kind, format, ttl, stale: StaleAction,
  license: License { terms_url, attribution: Option<String>, commercial: bool }, tenants: All|Only(Vec<TenantId>),
  key: Option<SecretRef>, auth: HttpAuth, origin: TiOrigin }`; `kind` — `Http { url }`, `Taxii { api_root,
  collection }`, `File`; `HttpAuth::{None, Header { name }, Basic { user }, Bearer, ClientCert}` — секрет из `key`
  (заголовок `X-Api-Key` у RST Cloud и `Auth-Key` у abuse.ch, Basic `taxii` у TAXII Kaspersky, сертификат клиента у
  HTTPS-фидов Kaspersky; решение 126 з–и); `TiOrigin::{Vendor, GosSopka, FinCert, Own}` — происхождение для журнала
  и отчётов (126 к); `format` — `Txt|Csv { column }|Rpz|MispFeed|Stix21`; `StaleAction::{Reduce { percent }, Report}`
  (по умолчанию `Reduce { 50 }`, «отключить» нет). `FORBIDDEN_BUNDLED` — константа хостов и имён фидов из решения
  126 ж, а также российских фидов (126 з) и `api.rstcloud.net` (126 и) до договора — снять запрет можно только
  решением владельца и изменением константы. `Bundled` с запрещённым источником, с `commercial: false` или с
  `origin` `GosSopka`/`FinCert` — `Invalid`; `Import` с `origin` `GosSopka`/`FinCert` без `tenants: Only` — `Invalid`.
  У версии списка — маркировка TLP из источника (`Tlp::{Clear, Green, Amber, AmberStrict, Red}`, `Option`).
- `RiskStore`: пакеты (`put_package`, `activate`, `rollback(version)`, `packages()` — активный и N прошлых,
  `risk.package.keep`), `policy(level)`/`set_policy`/`reset_policy`, источники (`sources`, `put_source`,
  `delete_source`), версии списков (`put_list_version { source, sha256, blob, entries, fetched_at }`, `list_current`),
  учёт (`record_usage(source, tenant, lookups, hits)` по суткам, `usage(source, since)`), срабатывания
  (`record_hits(tenant, message, &Verdict)`, `hits(tenant, filter)`). Событие шины `risk_changed` — сброс кэша узла.
- Sieve (`sieve.rs`): в контексте правил — `risk: Verdict` до Sieve и факты для `environment` (`thread_trusted`,
  `reply`); в итоге правил — `signals: Vec<(SignalId, Option<u16>)>` от `signal`.
- Описание — раздел «Risk» в `docs/CONTRACTS.md` и строка CHANGELOG. `tp-testkit`: `MemRisk` и набор `suite::risk`
  — его же проходит `PgMeta` (META-87).

## Как делают другие
- Символ с баллом и порогами действий — Rspamd (`actions`, https://docs.rspamd.com/configuration/metrics); score
  правила — SpamAssassin (https://spamassassin.apache.org/full/4.0.x/doc/Mail_SpamAssassin_Conf.html).
- Вендорские правила отдельно от локальных, обновление сохраняет баллы администратора — Stalwart
  (https://stalw.art/docs/spamfilter/rules/).
- Подписанные карты с откатом на локальный файл — Rspamd (https://docs.rspamd.com/configuration/maps).
- Отличие: детекторы — типизированный Rust-код, а не произвольные выражения или Lua; организация может только
  ужесточить; признак «только отчёт» — как ClassifySpam Stalwart, но с журналом срабатываний.

## Приёмка
- Тесты `effective`: без политик — значения пакета; установка снижает вес до 0 — признак не влияет на балл, но
  попадает в `hits`; организация снижает вес ниже установки — `validate` отклоняет с полями ошибки.
- `apply_update`: изменение администратора переживает обновление; признак, удалённый из пакета, — в
  `orphaned_overrides`; новый признак — `Mode::Report`.
- `Verdict::combine`: самое строгое из балла, действий признаков и Rspamd; `Report`-признак не меняет балл.
- `TiSource` `Bundled` с хостом из `FORBIDDEN_BUNDLED` (в том числе `taxii.tip.kaspersky.com`, `api.rstcloud.net`) —
  `Invalid`; `Bundled` с `origin: FinCert` — `Invalid`; `Import` с `origin: GosSopka` и `tenants: All` — `Invalid`;
  `StaleAction` без варианта «отключить».
- Набор `suite::risk` проходит `MemRisk`: пакеты растут по номеру, `rollback` на хранимую версию, хранится не больше
  N прошлых, чужая организация не видит политику и срабатывания другой.
