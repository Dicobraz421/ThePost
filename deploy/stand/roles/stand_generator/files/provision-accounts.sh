#!/bin/bash
# Учётные записи стенда SMB через thepost admin на узле (DEP-12, docs/INSTALL.md «Учётные записи»): сессия Admin API
# (admin init → admin login; повтор — пропуск по 409, при утраченной сессии — init --force), затем каждой учётной
# записи — account create (существующая — 409, пропуск) и пароль приложения (app-password create). Повтор пропускает
# учётные записи, чей пароль уже выпущен.
# Запуск (root): provision-accounts.sh <каталог состояния> <домен> <префикс> <число> [служебные имена…]
# Состояние (0600): <каталог>/session — сессия tps_…, <каталог>/accounts — строки login:пароль.
# Вывод — только счётчики: created=… existing=… issued=… skipped=… total=…; пароли не выводятся.
set -euo pipefail

STATE=$1 DOMAIN=$2 PREFIX=$3 COUNT=$4
shift 4
umask 077
mkdir -p "$STATE"
chmod 0700 "$STATE"
SESSION=$STATE/session
ACCOUNTS=$STATE/accounts
touch "$ACCOUNTS"

admin() { THEPOST_ADMIN_TOKEN=$TOKEN thepost admin --insecure "$@"; }
json() { python3 -c 'import json, sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }

TOKEN=$(cat "$SESSION" 2>/dev/null || true)
if [ -z "$TOKEN" ] || ! admin whoami >/dev/null 2>&1; then
  if ! out=$(thepost admin init 2>&1); then
    case $out in
      *409*)
        echo "admin init: администратор уже заведён (409), сессии стенда нет — admin init --force" >&2
        out=$(thepost admin init --force)
        ;;
      *)
        echo "$out" >&2
        exit 1
        ;;
    esac
  fi
  TOKEN=$(sed -n 's/^Токен Admin API.*: \(tpa_[^[:space:]]*\)$/\1/p' <<<"$out")
  [ -n "$TOKEN" ] || { echo "admin init: в выводе нет токена Admin API" >&2; exit 1; }
  TOKEN=$(admin login --json | json session)
  printf '%s\n' "$TOKEN" >"$SESSION"
fi

created=0 existing=0 issued=0 skipped=0
logins=()
for ((n = 1; n <= COUNT; n++)); do logins+=("$PREFIX$n@$DOMAIN"); done
for name in "$@"; do logins+=("$name@$DOMAIN"); done
for login in "${logins[@]}"; do
  if grep -q "^$login:" "$ACCOUNTS"; then
    skipped=$((skipped + 1))
    continue
  fi
  if err=$(admin account create "$login" 2>&1 >/dev/null); then
    created=$((created + 1))
  elif [[ $err == *409* ]]; then
    existing=$((existing + 1))
  else
    echo "$login: $err" >&2
    exit 1
  fi
  pw=$(admin app-password create "$login" --label stand --json | json password)
  printf '%s:%s\n' "$login" "$pw" >>"$ACCOUNTS"
  issued=$((issued + 1))
done
echo "created=$created existing=$existing issued=$issued skipped=$skipped total=${#logins[@]}"
