#!/bin/bash
# Сценарий нагрузки стенда SMB по модели APP-D Д.3 (сценарии — Д.6): входящая почта (smtp-source → 25),
# исходящая (swaks → 587 с AUTH; доля внутренним адресатам, остальные — внешние через smtp-sink генератора),
# IMAP (imaptest: подключённые клиенты, чтение, флаги, поиск), JMAP (k6: push, поиск, флаги).
# Запуск: run-scenario.sh <base|peak|long> [длительность, с] [множитель]. Параметры — /etc/thepost-stand/stand.env.
# Отчёт — $STAND_DIR/reports/<RUN_ID>/ (summary.txt и журналы инструментов).
set -euo pipefail

# shellcheck source=/dev/null
. /etc/thepost-stand/stand.env

SCENARIO=${1:-base}
var="SCENARIO_${SCENARIO^^}"
[ -n "${!var:-}" ] || { echo "неизвестный сценарий: $SCENARIO" >&2; exit 2; }
read -r FACTOR DURATION <<<"${!var}"
DURATION=${2:-$DURATION}
FACTOR=${3:-$FACTOR}
RUN=${RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)}-$SCENARIO
OUT=$STAND_DIR/reports/$RUN
CORPUS=$STAND_DIR/corpus
mkdir -p "$OUT"
if [ -n "$CA_FILE" ]; then export SSL_CERT_FILE=$CA_FILE CURL_CA_BUNDLE=$CA_FILE; fi

# Частота в секунду: <в сутки на пользователя> × доля × пользователи × множитель / рабочие часы.
rate() { awk -v d="$1" -v s="${2:-1}" -v u="$USERS" -v f="$FACTOR" -v h="$WORKDAY_HOURS" \
  'BEGIN { printf "%.6f", d * s * u * f / (h * 3600) }'; }
IN_RATE=$(rate "$INBOUND_PER_DAY")
OUT_RATE=$(rate "$OUTBOUND_PER_DAY")
SEARCH_PER_MIN=$(awk -v r="$(rate "$SEARCHES_PER_DAY")" 'BEGIN { printf "%.4f", r * 60 }')
FLAGS_PER_MIN=$(awk -v r="$(rate "$FLAG_CHANGES_PER_DAY")" 'BEGIN { printf "%.4f", r * 60 }')
# Подключены в пиковый час: доля пользователей; устройство IMAP (IDLE) — одно, остальные — JMAP push.
IMAP_CLIENTS=$(awk -v u="$USERS" -v o="$ONLINE_SHARE" 'BEGIN { printf "%d", u * o }')
PUSH_VUS=$(awk -v u="$USERS" -v o="$ONLINE_SHARE" -v d="$DEVICES_PER_USER" 'BEGIN { printf "%d", u * o * (d - 1) }')

mapfile -t MSGS < <(ls "$CORPUS"/msg-*.eml)
user() { echo "${USER_PREFIX}$1@${DOMAIN}"; }
rnd_user() { echo $(( (RANDOM * 32768 + RANDOM) % USERS + 1 )); }
rnd_msg() { echo "${MSGS[$(( (RANDOM * 32768 + RANDOM) % ${#MSGS[@]} ))]}"; }

send_in() {
  smtp-source -s 1 -m 1 -F "$(rnd_msg)" -f "sender@${EXTERNAL_DOMAIN}" -t "$(user "$(rnd_user)")" "${SMB_HOST}:25"
}

send_out() {
  local from to
  from=$(user "$(rnd_user)")
  if awk -v s="$OUTBOUND_INTERNAL_SHARE" -v r="$RANDOM" 'BEGIN { exit !(r / 32768 < s) }'; then
    to=$(user "$(rnd_user)")
  else
    to="sink$RANDOM@${EXTERNAL_DOMAIN}"
  fi
  swaks --silent 2 --server "$SMB_HOST" --port 587 --tls --auth PLAIN --auth-user "$from" \
    --auth-password "$USER_PASSWORD" --from "$from" --to "$to" --data "@$(rnd_msg)"
}

# Запуск команды с частотой <в секунду> в течение <с>; итог каждого вызова — строка ok/fail в журнале.
paced() {
  local rate=$1 dur=$2 log=$3
  shift 3
  local step start end next now i=0
  step=$(awk -v r="$rate" 'BEGIN { if (r <= 0) print 0; else printf "%d", 1e9 / r }')
  [ "$step" -gt 0 ] || return 0
  start=$(date +%s%N)
  end=$((start + dur * 1000000000))
  while next=$((start + i * step)); [ "$next" -lt "$end" ]; do
    now=$(date +%s%N)
    if [ "$next" -gt "$now" ]; then sleep "$(awk -v d=$((next - now)) 'BEGIN { printf "%.3f", d / 1e9 }')"; fi
    ( if "$@" >/dev/null 2>&1; then echo ok; else echo fail; fi ) >>"$log" &
    i=$((i + 1))
  done
  wait
}

cat >"$OUT/params.txt" <<EOF
run=$RUN scenario=$SCENARIO factor=$FACTOR duration=${DURATION}s users=$USERS host=$SMB_HOST
inbound_per_sec=$IN_RATE outbound_per_sec=$OUT_RATE search_per_min=$SEARCH_PER_MIN flags_per_min=$FLAGS_PER_MIN
imap_clients=$IMAP_CLIENTS push_connections=$PUSH_VUS corpus_messages=${#MSGS[@]}
EOF

# Учётные записи заведены: вход IMAP первой из них.
if ! curl -sS --max-time 30 --user "$(user 1):$USER_PASSWORD" "imaps://$SMB_HOST/" >/dev/null 2>"$OUT/preflight.log"; then
  echo "вход IMAP $(user 1) не удался: учётные записи стенда не заведены или узел недоступен ($OUT/preflight.log)" >&2
  exit 3
fi

SINK_BEFORE=$(find "$STAND_DIR/sink" -type f | wc -l)
: >"$OUT/smtp-in.log"
: >"$OUT/submission.log"
LIMIT=$((DURATION + 600))
declare -A JOBS

paced "$IN_RATE" "$DURATION" "$OUT/smtp-in.log" send_in &
JOBS[smtp-in]=$!
paced "$OUT_RATE" "$DURATION" "$OUT/submission.log" send_out &
JOBS[submission]=$!
timeout "$LIMIT" podman run --rm --network host -v "$CORPUS:/corpus:ro" "$IMAPTEST_IMAGE" \
  host="$SMB_ADDRESS" port=993 user="${USER_PREFIX}%d@${DOMAIN}" pass="$USER_PASSWORD" users="$USERS" \
  clients="$IMAP_CLIENTS" secs="$DURATION" mbox=/corpus/imaptest.mbox $IMAPTEST_ARGS >"$OUT/imaptest.log" 2>&1 &
JOBS[imaptest]=$!
SMB_HOST=$SMB_HOST DOMAIN=$DOMAIN USERS=$USERS USER_PREFIX=$USER_PREFIX USER_PASSWORD=$USER_PASSWORD \
  DURATION=$DURATION PUSH_VUS=$PUSH_VUS SEARCH_PER_MIN=$SEARCH_PER_MIN FLAGS_PER_MIN=$FLAGS_PER_MIN \
  WORDS_FILE=$CORPUS/words.txt timeout "$LIMIT" k6 run --quiet --summary-export "$OUT/k6-summary.json" \
  "$STAND_DIR/jmap.js" >"$OUT/k6.log" 2>&1 &
JOBS[k6]=$!

: >"$OUT/exit-codes.txt"
for name in "${!JOBS[@]}"; do
  code=0
  wait "${JOBS[$name]}" || code=$?
  echo "$name=$code" >>"$OUT/exit-codes.txt"
done
# Хвост очереди: письма внешним адресатам доходят до smtp-sink после конца нагрузки.
sleep 60
SINK_AFTER=$(find "$STAND_DIR/sink" -type f | wc -l)

count() { grep -c "^$2\$" "$1" || true; }
{
  cat "$OUT/params.txt"
  echo "smtp-in: ok=$(count "$OUT/smtp-in.log" ok) fail=$(count "$OUT/smtp-in.log" fail)"
  echo "submission: ok=$(count "$OUT/submission.log" ok) fail=$(count "$OUT/submission.log" fail)"
  echo "smtp-sink (внешние адресаты): received=$((SINK_AFTER - SINK_BEFORE))"
  echo "exit codes: $(tr '\n' ' ' <"$OUT/exit-codes.txt")"
  if [ -s "$OUT/k6-summary.json" ]; then
    python3 - "$OUT/k6-summary.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))["metrics"]
d = m.get("http_req_duration", {})
f = m.get("http_req_failed", {})
c = m.get("checks", {})
print("k6: http_req_duration p95={:.1f}ms p99={:.1f}ms; http_req_failed={:.4f}; checks passed={}, failed={}".format(
    d.get("p(95)", 0), d.get("p(99)", 0), f.get("value", f.get("rate", 0)), c.get("passes", 0), c.get("fails", 0)))
PY
  fi
  echo "imaptest (последние строки):"
  tail -n 15 "$OUT/imaptest.log" || true
} >"$OUT/summary.txt"
cat "$OUT/summary.txt"
echo "report=$OUT"
