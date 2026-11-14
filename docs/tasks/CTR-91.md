# CTR-91: брендинг заказчика в контракте — `BrandingStore`, роли цветов, уровни установки и организации

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/branding.rs, crates/tp-contracts/src/branding/*, crates/tp-contracts/src/lib.rs, docs/CONTRACTS.md, crates/tp-testkit/*, docs/decisions/2026-11-11-stage2-followups.md, docs/tasks/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#122, design/tokens.toml, crates/tp-contracts/src/sharing.rs, crates/tp-contracts/src/blob.rs]
checks: [nextest]
manual: no

## Сделать
- Модуль `branding.rs`: `BrandColor` — цвет `#RRGGBB` (`FromStr`, `Display` в верхнем регистре, serde строкой; иное —
  `Error::Invalid`), контраст и яркость WCAG; `BrandLogo { hash: BlobHash, size, width, height }` — знак PNG, путь в
  BLOB `BrandLogo::path()` = `branding/<hash>.png`; `BrandingScope` — `Installation` или `Org(TenantId)`.
- Роли — из `design/tokens.toml`: `Role` (`[roles]`, по темам `Theme::{Light, Dark}`) и `CliRole` (`[cli.roles]`);
  цвета IT TASK — `Role::it_task(theme)`, `CliRole::it_task()`, `Palette::it_task()`; пары `CONTRAST` —
  `[[contrast]]` (тест сверяет всё с файлом токенов). Порог — 4,5 у текста, 3 у рамок и заливок, 3 у ролей CLI на обоих
  фонах терминала, без исключений; розовый выделения IT TASK — `#E3107F` (решение 122).
- `Branding { product_name?, primary?, light, dark, cli, logo? }` — обязательных полей нет; `validate()` — название
  1–64 символа без управляющих, знак до 256 КБ и 1024×1024. `Branding::palette_over(parent)` — чистая функция вывода:
  заданная роль как есть; при основном цвете уровня роли бренда (`frameA`, `fkBg`, `sel`, `head`, `titleA`, текст на
  заливках, CLI `head`, `json_key`, `accent`, `json_bool`) выводятся из него с подгонкой до порогов; прочие — с уровня
  выше; уровень без цветов — цвета уровня выше целиком.
- `BrandingLevels { installation, org }::effective()` — каждое поле с ближайшего уровня: организация → установка →
  IT TASK; `Palette::contrast_issues()` — пары ниже порога (тема, роль, фон, контраст, порог). Проверка при
  сохранении — `BrandingLevels::contrast_issues()` (организация поверх установки) и `installation_change_issues`
  (новый набор установки и действующие палитры всех организаций); страховка отрисовки — `Palette::guarded`,
  `Branding::safe_palette_over` (пара ниже порога — с уровня выше), ею пользуется `effective()`.
- `BrandingStore`: `branding(scope)`, `set_branding(scope, &Branding)` (замена уровня целиком, `validate()`),
  `reset_branding(scope)` (возвращает прежний; знак удаляет вызывающий), `branding_levels(Option<TenantId>)` (оба
  уровня одним чтением), `org_brandings()` (свои наборы всех организаций), `logo_in_use(hash)`. Контраст проверяет вызывающий по действующему набору (ADM-48).
- Описание — раздел «Branding» в `docs/CONTRACTS.md` и строка CHANGELOG; решение 122 и карточки META-84, ADM-48, UI-22,
  TUI-08, DOC-11 — по решениям владельца 14.11 (все роли, обязательных полей нет, без исключения 4,48).
- `tp-testkit`: `MemBranding` и набор `suite::branding` — его же проходит `PgMeta` (META-84).

## Приёмка
- Тест набора: без записей — уровней нет; набор установки виден каждой организации; свой набор организации виден только
  ей и заменяется целиком; организация сменила только название — знак и цвета с установки; сброс организации
  возвращает установку; `logo_in_use` после сброса последнего уровня — `false`.
- Тесты контракта: `#177CAA` и `#e4107f` разбираются, `#FFF`, `177CAA`, `#GGGGGG` — `Invalid`; IT TASK проходит все
  пары; любой основной цвет (сетка 6×6×6) даёт палитру без нарушений; заданная роль не сдвигается, нарушение называет
  роль и пару; роли и цвета совпадают с `design/tokens.toml`.
