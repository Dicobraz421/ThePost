# DEP-35: Dockerfile: образ с веб-консолью

lane: deploy
deps: [DEPS-07]
class: A
files: [deploy/docker/Dockerfile]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S7.md#7.1]
checks: [none]
manual: no

Часть CI-08 в полосе deploy, выделена по решению 71 (карточка — одна полоса).

## Сделать
- `deploy/docker/Dockerfile`: этап сборки консоли (Node — версия из `console/package.json`, решение 49) до `cargo build`,
  `console/dist` — в контекст сборки Rust; фича `console` (DEPS-07) — аргументом сборки, по умолчанию включена.
- Образ без консоли — тем же файлом с аргументом `--build-arg CONSOLE=0` (для CI-08 и сборок без Node).

## Приёмка
- `docker build` локально с консолью и без; в образе с консолью `GET /console/` отдаёт страницу консоли.
