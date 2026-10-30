#!/bin/sh
# Начальная настройка OpenBao в Kubernetes для The Post: KV v2 `thepost`, политика `thepost`
# (deploy/openbao/thepost-policy.hcl), вход Kubernetes auth для сервисного аккаунта thepost, пароль роли PostgreSQL.
# Запуск — после `bao operator init` (ключи восстановления и root-токен остаются у администратора):
#   BAO_TOKEN=<токен администратора> sh deploy/helm/openbao/bootstrap.sh [<пространство OpenBao> [<пространство thepost>]]
# Повторный запуск ничего не меняет. Токен передаётся в под через stdin, не аргументами.
set -eu

NS=${1:-openbao}
APP_NS=${2:-thepost}
POD=${OPENBAO_POD:-openbao-0}
POLICY="$(dirname "$0")/../../openbao/thepost-policy.hcl"
: "${BAO_TOKEN:?нужен BAO_TOKEN (root-токен или токен администратора)}"

{
  printf 'set -eu\nexport BAO_TOKEN=%s\nAPP_NS=%s\n' "$BAO_TOKEN" "$APP_NS"
  cat <<'EOF'
bao secrets list | grep -q '^thepost/' || bao secrets enable -path=thepost -version=2 kv
bao policy write thepost - <<'POLICY'
EOF
  cat "$POLICY"
  cat <<'EOF'
POLICY
bao auth list | grep -q '^kubernetes/' || bao auth enable kubernetes
# Внутри кластера: CA и токен для TokenReview — сервисного аккаунта OpenBao.
bao write auth/kubernetes/config kubernetes_host="https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT" >/dev/null
# Токен узла живёт час и продлевается до суток, затем — новый вход.
bao write auth/kubernetes/role/thepost bound_service_account_names=thepost \
  bound_service_account_namespaces="$APP_NS" token_policies=thepost token_ttl=1h token_max_ttl=24h >/dev/null
# Пароль роли thepost в PostgreSQL — случайный при первом запуске; в СУБД его задаёт администратор (deploy/README.md).
if ! bao kv get -mount=thepost db >/dev/null 2>&1; then
  head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' | bao kv put -mount=thepost db username=thepost password=- >/dev/null
  echo "bootstrap: записан секрет thepost/db"
fi
echo "bootstrap: KV thepost, политика thepost, Kubernetes auth (роль thepost, пространство $APP_NS) — готово"
EOF
} | kubectl exec -i -n "$NS" "$POD" -- sh -s
