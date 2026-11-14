# CTR-91: брендинг заказчика в контракте — `BrandingStore`, уровни установки и организации

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/branding.rs, crates/tp-contracts/src/lib.rs, docs/CONTRACTS.md, crates/tp-testkit/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#122, crates/tp-contracts/src/sharing.rs, crates/tp-contracts/src/blob.rs]
checks: [nextest]
manual: no

## Сделать
- Модуль `branding.rs`: `BrandColor` — цвет `#RRGGBB` (`FromStr`, `Display` в верхнем регистре, serde строкой; иное —
  `Error::Invalid`); `BrandLogo { hash: BlobHash, size, width, height }` — знак PNG, путь в BLOB
  `BrandLogo::path()` = `branding/<hash>.png` (`BlobStore::put_raw`: путь по содержимому неизменяем);
  `Branding { product_name, primary, accent, logo: Option<BrandLogo> }`, `validate()` — название 1–64 символа без
  управляющих, знак не больше 256 КБ и 1024×1024; `BrandingScope` — `Installation` или `Org(TenantId)`.
- `BrandingStore`: `branding(scope)` — свой набор уровня или `None`; `set_branding(scope, &Branding)` — заменяет набор
  уровня целиком (проверяет `validate()`); `reset_branding(scope)` — удаляет и возвращает прежний (знак удаляет
  вызывающий); `effective_branding(Option<TenantId>)` — ближайший заданный уровень целиком: свой набор организации,
  иначе установки, иначе `None` (встроенный IT TASK); `logo_in_use(hash)` — знак упомянут хоть одним уровнем.
- Контраст и цвета ролей — не в контракте (Admin API, ADM-48). Описание — раздел «Branding» в `docs/CONTRACTS.md` и
  строка CHANGELOG.
- `tp-testkit`: `MemBranding` и набор `suite::branding` — его же проходит `PgMeta` (META-84).

## Приёмка
- Тест набора: без записей — `effective_branding` `None`; уровень установки виден организации без своего набора;
  свой набор организации заменяет установку целиком (без знака — без знака, а не знак установки); сброс организации
  возвращает установку; чужая организация своего набора не видит; `logo_in_use` после сброса последнего уровня — `false`.
- Тесты `BrandColor`: `#177CAA` и `#e4107f` разбираются (вывод — верхний регистр), `#FFF`, `177CAA`, `#GGGGGG` — `Invalid`.
