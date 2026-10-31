# Разработка The Post

## Окружение
- Linux x86_64/AArch64 (на Windows — WSL2 Ubuntu). Rust stable (`rust-toolchain.toml`), `cargo-nextest`.
- Зависимости для интеграционных тестов: `docker compose -f deploy/compose/dev.yml up -d` (после DEP-01).
- Переменные: `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres`,
  `TEST_S3_URL`, `TEST_VALKEY_URL`, `TEST_LDAP_URL` — тесты без переменной пропускаются (`tp_testkit::pg_url()`).

## Проверки (конец волны)
```
cargo fmt --all
cargo clippy -p <крейты> --all-targets -- -D warnings
cargo nextest run -p <крейты> --no-tests=pass
```
CI (`.github/workflows/ci.yml`): changes → fmt, clippy, test (PostgreSQL 16), deny (при изменении зависимостей) → `gate`.
Release-сборка — только main, `beta-*`, ручной запуск.

## Агенты
Все полосы собираются на Linux, поэтому основной режим — облачные агенты (Managed Agents).
```
python scripts/cloud_agent.py run ThePost <полоса> <NN> <ID> [<ID> ...] [--budget 10]
python scripts/cloud_agent.py wait <session>
gh pr create -R Dicobraz421/ThePost --head w/<полоса>-<NN> --fill && gh pr merge -R Dicobraz421/ThePost --auto w/<полоса>-<NN>
```
Нужны `ant` (`ant auth login`) и `gh` (`gh auth login`). Агент и окружение — те же, что у pets (`PETS_AGENT_ID`, `PETS_AGENT_ENV`).
Альтернатива — сессия Claude Code (claude.ai/code или локально в WSL2) с промптом `docs/process/agent-prompt.md`;
локально — отдельный worktree на полосу: `git worktree add ../ThePost-<полоса> origin/main`, свой `CARGO_TARGET_DIR`, общий `sccache`.

## Карточки
Шаблон — поля `lane, deps, class, files, read, checks, manual` + разделы «Сделать», «Приёмка» (см. любую `docs/tasks/*.md`).
Ручная проверка — `docs/manual-tests/<ID>.md`. Решение владельца — `docs/decisions/<YYYY-MM-DD>-<slug>.md`.
