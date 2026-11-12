# DEP-50: Helm и Compose: закрытый образ и тег версии

lane: deploy
deps: [CI-13]
class: A
files: [deploy/helm/thepost/*, deploy/compose/*, docs/INSTALL.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Чарт по умолчанию берёт тег из `appVersion`; `imagePullSecrets` — обязательный параметр с понятной ошибкой, если
  не задан; Compose — `docker login ghcr.io` в инструкции (решения 110, 111). INSTALL — отдельным PR полосы docs.

## Приёмка
- `helm template` без `imagePullSecrets` падает с подсказкой; с ним — манифест с секретом.
