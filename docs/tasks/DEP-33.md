# DEP-33: OpenBao в Kubernetes без `bootstrap.sh`

lane: deploy
deps: [DEP-10]
class: A
files: [deploy/helm/*, deploy/README.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [none]
manual: no

Хвост волны ci-04 (CI-05), PR #350; решение 6: `bootstrap.sh` Helm-чарта OpenBao — заменить (этап 3, полоса deploy).

## Сделать
- Настройка Kubernetes auth и роли `thepost` в OpenBao без своего сценария на shell — готовым средством (Job Helm-хука
  на образе OpenBao с командами `bao` в манифесте или декларативная настройка); выбор — в PR с обоснованием;
  `deploy/helm/openbao/bootstrap.sh` удалить, README и комментарий в `values.yaml` — обновить.

## Приёмка
- `helm template`, kubeconform без ошибок; повторная установка не меняет настройку.
