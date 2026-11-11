#!/bin/bash
# Сценарий нагрузки стенда SMB по модели APP-D Д.3 (сценарии — Д.6, критерии — Д.7): входящая почта (smtp-source → 25),
# исходящая (swaks → 587 с AUTH; доля внутренним адресатам, остальные — внешние через smtp-sink генератора),
# IMAP (imaptest: подключённые клиенты, чтение, флаги, поиск), JMAP (k6: push, поиск, флаги); на всё время нагрузки —
# зонды Д.7 (probes.py, probe.js), до нагрузки — замер памяти простаивающих соединений, после — сверка принятых писем
# и отчёт criteria.py (criteria.md, criteria.json).
# Запуск: run-scenario.sh <base|peak|stress|x3|faults|long> [длительность, с] [множитель].
#   stress — ступени STRESS_FACTORS по STRESS_STEP с до первого нарушения Д.7 (criteria.py --check);
#   faults — отказы вносит stand.yml (kill -9 узла, перезапуск PostgreSQL) и пишет их в events.txt.
# Подключённые клиенты (imaptest, push) — тоже × множитель.
# Параметры — /etc/thepost-stand/stand.env, пароли — $ACCOUNTS (login:пароль). Отчёт — $STAND_DIR/reports/<RUN_ID>-…/.
# WAIT_SMB=1 (stand.yml): после нагрузки — load.done и ожидание smb.done (срезы узла smb-samples.csv в каталоге).
# Код выхода: 0 — все измеренные критерии прошли, 1 — есть «не прошёл», 2–3 — прогон не состоялся.
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
# stand.yml ждёт load.done: и при раннем выходе.
trap 'touch "$OUT/load.done"' EXIT
if [ -n "$CA_FILE" ]; then export SSL_CERT_FILE=$CA_FILE CURL_CA_BUNDLE=$CA_FILE; fi

# Пароли приложений: свой у каждой учётной записи; imaptest — файл учётных записей нагрузки.
declare -A PASS
while IFS=: read -r login pw; do [ -n "$login" ] && PASS[$login]=$pw; done <"$ACCOUNTS"
LOAD_USERS=$(dirname "$ACCOUNTS")/load-users
(umask 077 && awk -F'[@:]' -v p="$USER_PREFIX" -v n="$USERS" 'index($1, p) == 1 {
  k = substr($1, length(p) + 1); if (k ~ /^[0-9]+$/ && k + 0 >= 1 && k + 0 <= n) print }' "$ACCOUNTS" >"$LOAD_USERS")
if [ "$(wc -l <"$LOAD_USERS")" -lt "$USERS" ]; then
  echo "в $ACCOUNTS меньше $USERS учётных записей нагрузки: сначала --tags provision" >&2
  exit 3
fi

# Частота в секунду: <в сутки на пользователя> × пользователи × множитель / рабочие часы.
rate() { awk -v d="$1" -v u="$USERS" -v f="$2" -v h="$WORKDAY_HOURS" 'BEGIN { printf "%.6f", d * u * f / (h * 3600) }'; }
per_min() { awk -v r="$(rate "$1" "$2")" 'BEGIN { printf "%.4f", r * 60 }'; }
# Подключены в пиковый час: доля пользователей × множитель; устройство IMAP (IDLE) — одно, остальные — JMAP push.
imap_clients() { awk -v u="$USERS" -v o="$ONLINE_SHARE" -v f="$1" 'BEGIN { printf "%d", u * o * f }'; }
push_vus() { awk -v u="$USERS" -v o="$ONLINE_SHARE" -v d="$DEVICES_PER_USER" -v f="$1" 'BEGIN { printf "%d", u * o * (d - 1) * f }'; }
event() { echo "$(date +%s.%N | cut -c1-14) $*" >>"$OUT/events.txt"; }

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
    --auth-password "${PASS[$from]}" --from "$from" --to "$to" --data "@$(rnd_msg)"
}

# Итог команды: ok или fail <код SMTP 4xx/5xx; conn — без кода (соединение, TLS)>.
outcome() {
  local out code
  if out=$("$@" 2>&1); then echo ok; return; fi
  code=$(grep -oE '(^|[^0-9.])[45][0-9]{2}([ -]|$)' <<<"$out" | grep -oE '[45][0-9]{2}' | tail -n 1 || true)
  echo "fail ${code:-conn}"
}

# Запуск команды с частотой <в секунду> в течение <с>; итог каждого вызова — строка журнала (outcome).
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
    outcome "$@" >>"$log" &
    i=$((i + 1))
  done
  wait
}

# Нагрузка Д.3 с множителем <f> на <с>; журналы инструментов — в каталог <dir>.
load() {
  local f=$1 dur=$2 dir=$3 limit clients vus name code
  local -A jobs
  mkdir -p "$dir"
  clients=$(imap_clients "$f")
  vus=$(push_vus "$f")
  echo "start=$(date +%s) factor=$f duration=${dur}s inbound_per_sec=$(rate "$INBOUND_PER_DAY" "$f")" \
    "outbound_per_sec=$(rate "$OUTBOUND_PER_DAY" "$f") search_per_min=$(per_min "$SEARCHES_PER_DAY" "$f")" \
    "flags_per_min=$(per_min "$FLAG_CHANGES_PER_DAY" "$f") imap_clients=$clients push_connections=$vus" >"$dir/load.txt"
  : >"$dir/smtp-in.log"
  : >"$dir/submission.log"
  limit=$((dur + 600))
  paced "$(rate "$INBOUND_PER_DAY" "$f")" "$dur" "$dir/smtp-in.log" send_in &
  jobs[smtp-in]=$!
  paced "$(rate "$OUTBOUND_PER_DAY" "$f")" "$dur" "$dir/submission.log" send_out &
  jobs[submission]=$!
  if [ "$clients" -gt 0 ]; then
    # Файл паролей — 0600 root: imaptest (в образе — пользователь mail) запускается от root контейнера. Имена —
    # шаблон user= и users=, пароли — userfile; сертификат проверяется (имя узла, CA внутреннего режима).
    local ca=()
    if [ -n "$CA_FILE" ]; then ca=(-v "$CA_FILE:/ca.pem:ro"); fi
    # shellcheck disable=SC2086 # IMAPTEST_ARGS — набор слов key=value
    timeout "$limit" podman run --rm --network host --user 0:0 -v "$CORPUS:/corpus:ro" -v "$LOAD_USERS:/users:ro" \
      "${ca[@]}" "$IMAPTEST_IMAGE" host="$SMB_HOST" port=993 user="${USER_PREFIX}%d@${DOMAIN}" users="1-$USERS" \
      userfile=/users clients="$clients" secs="$dur" mbox=/corpus/imaptest.mbox $IMAPTEST_ARGS \
      ${CA_FILE:+ssl_ca_file=/ca.pem} >"$dir/imaptest.log" 2>&1 &
    jobs[imaptest]=$!
  fi
  SMB_HOST=$SMB_HOST DOMAIN=$DOMAIN USERS=$USERS USER_PREFIX=$USER_PREFIX ACCOUNTS=$ACCOUNTS DURATION=$dur \
    PUSH_VUS=$vus SEARCH_PER_MIN=$(per_min "$SEARCHES_PER_DAY" "$f") FLAGS_PER_MIN=$(per_min "$FLAG_CHANGES_PER_DAY" "$f") \
    WORDS_FILE=$CORPUS/words.txt timeout "$limit" k6 run --quiet --summary-export "$dir/k6-summary.json" \
    "$STAND_DIR/jmap.js" >"$dir/k6.log" 2>&1 &
  jobs[k6]=$!
  : >"$dir/exit-codes.txt"
  for name in "${!jobs[@]}"; do
    code=0
    wait "${jobs[$name]}" || code=$?
    echo "$name=$code" >>"$dir/exit-codes.txt"
  done
}

PROBE_ARGS=(--accounts "$ACCOUNTS" --host "$SMB_HOST" --domain "$DOMAIN" --prefix "$USER_PREFIX" --users "$USERS"
  --big "$BIG_MAILBOX" --ca "$CA_FILE")
K6_ENV=(SMB_HOST="$SMB_HOST" DOMAIN="$DOMAIN" USERS="$USERS" USER_PREFIX="$USER_PREFIX" ACCOUNTS="$ACCOUNTS"
  WORDS_FILE="$CORPUS/words.txt" BIG="$BIG_MAILBOX" INTERVAL="$PROBE_INTERVAL")
count_mail() { python3 "$STAND_DIR/probes.py" count "${PROBE_ARGS[@]}" --out-json "$1"; }
sink() { find "$STAND_DIR/sink" -type f | wc -l; }

cat >"$OUT/params.txt" <<EOF
run=$RUN scenario=$SCENARIO factor=$FACTOR duration=${DURATION}s users=$USERS host=$SMB_HOST
imap_clients=$(imap_clients "$FACTOR") push_connections=$(push_vus "$FACTOR") corpus_messages=${#MSGS[@]}
probe_interval=$PROBE_INTERVAL idle_connections=$IDLE_CONNECTIONS idle_hold=$IDLE_HOLD long_window=$LONG_WINDOW
stress_factors=${STRESS_FACTORS// /,} stress_step=$STRESS_STEP db_pool=$DB_POOL
EOF

# Учётные записи заведены: вход IMAP первой из них.
if ! curl -sS --max-time 30 --user "$(user 1):${PASS[$(user 1)]}" "imaps://$SMB_HOST/" >/dev/null 2>"$OUT/preflight.log"; then
  echo "вход IMAP $(user 1) не удался: учётные записи стенда не заведены или узел недоступен ($OUT/preflight.log)" >&2
  exit 3
fi
: >"$OUT/events.txt"

# Память простаивающих соединений (Д.7): N IMAP IDLE, затем N JMAP push; RSS узла — срезы smb-sampler.py.
if [ "$IDLE_CONNECTIONS" -gt 0 ]; then
  python3 "$STAND_DIR/probes.py" idle "${PROBE_ARGS[@]}" -n "$IDLE_CONNECTIONS" --hold "$IDLE_HOLD" \
    --events "$OUT/events.txt" >"$OUT/idle-imap.log" 2>&1 || echo "idle IMAP: код $?" >>"$OUT/idle-imap.log"
  sleep 15
  event idle_push_open
  until=$(($(date +%s) + 30 + IDLE_HOLD))
  env "${K6_ENV[@]}" MODE=idle IDLE_N="$IDLE_CONNECTIONS" IDLE_HOLD="$IDLE_HOLD" IDLE_UNTIL="$until" \
    k6 run --quiet --no-summary --log-format raw --console-output "$OUT/probe-idle-k6.jsonl" \
    "$STAND_DIR/probe.js" >"$OUT/idle-push.log" 2>&1 || echo "idle push: код $?" >>"$OUT/idle-push.log"
  event idle_push_close
  sleep 10
fi

count_mail "$OUT/count-before.json" >/dev/null
SINK_BEFORE=$(sink)
rm -f "$OUT/probes.stop"
python3 "$STAND_DIR/probes.py" run "${PROBE_ARGS[@]}" --interval "$PROBE_INTERVAL" --out "$OUT/probes.jsonl" \
  --stop "$OUT/probes.stop" >"$OUT/probes.log" 2>&1 &
PROBES=$!
for _ in $(seq 1 120); do grep -q probes_start "$OUT/probes.jsonl" 2>/dev/null && break; sleep 1; done
env "${K6_ENV[@]}" MODE=run DURATION=$((86400 * 30)) k6 run --quiet --no-summary --log-format raw \
  --console-output "$OUT/probe-k6.jsonl" "$STAND_DIR/probe.js" >"$OUT/probe-k6.log" 2>&1 &
K6PROBE=$!
sleep 5

event load_start
touch "$OUT/load.started"
if [ "$SCENARIO" = stress ]; then
  i=0
  for f in $STRESS_FACTORS; do
    i=$((i + 1))
    event step_start "factor=$f"
    load "$f" "$STRESS_STEP" "$OUT/step-$i-x$f"
    event step_end "factor=$f"
    t0=$(awk -v f="factor=$f" '$2 == "step_start" && $3 == f { print $1 }' "$OUT/events.txt" | tail -n 1)
    t1=$(awk -v f="factor=$f" '$2 == "step_end" && $3 == f { print $1 }' "$OUT/events.txt" | tail -n 1)
    if ! python3 "$STAND_DIR/criteria.py" "$OUT" --window "$t0" "$t1" --check >"$OUT/step-$i-x$f/violations.txt"; then
      event step_violation "factor=$f"
      break
    fi
  done
else
  load "$FACTOR" "$DURATION" "$OUT"
fi
event load_end

touch "$OUT/probes.stop"
kill -INT "$K6PROBE" 2>/dev/null || true
wait "$K6PROBE" || true
wait "$PROBES" || echo "probes: код $?" >>"$OUT/probes.log"

# Сверка: принято 250 (нагрузка и зонды) = в ящиках + в smtp-sink; хвост очереди — до TAIL_MAX с.
accepted=$(( $(cat "$OUT"/smtp-in.log "$OUT"/submission.log "$OUT"/step-*/smtp-in.log "$OUT"/step-*/submission.log \
  2>/dev/null | grep -c '^ok$' || true) + $(grep -c '"accepted": 1' "$OUT/probes.jsonl" || true) ))
before=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["total"])' "$OUT/count-before.json")
deadline=$(($(date +%s) + TAIL_MAX))
sleep 30
while :; do
  after=$(count_mail "$OUT/count-after.json")
  boxes=$((after - before))
  sunk=$(($(sink) - SINK_BEFORE))
  lost=$((accepted - boxes - sunk))
  if [ "$lost" -le 0 ] || [ "$(date +%s)" -ge "$deadline" ]; then break; fi
  sleep 30
done
echo "accepted=$accepted mailboxes=$boxes sink=$sunk lost=$lost" >"$OUT/ledger.txt"

touch "$OUT/load.done"
if [ -n "${WAIT_SMB:-}" ]; then
  for _ in $(seq 1 600); do [ -e "$OUT/smb.done" ] && break; sleep 1; done
fi

code=0
python3 "$STAND_DIR/criteria.py" "$OUT" >"$OUT/criteria.txt" 2>&1 || code=$?
{
  cat "$OUT/params.txt"
  echo "ledger: $(cat "$OUT/ledger.txt")"
  for dir in "$OUT" "$OUT"/step-*; do
    [ -e "$dir/load.txt" ] || continue
    echo "load $(basename "$dir"): $(cat "$dir/load.txt")"
    echo "  smtp-in: ok=$(grep -c '^ok$' "$dir/smtp-in.log" || true) fail=$(grep -c '^fail' "$dir/smtp-in.log" || true);" \
      "submission: ok=$(grep -c '^ok$' "$dir/submission.log" || true) fail=$(grep -c '^fail' "$dir/submission.log" || true);" \
      "exit codes: $(tr '\n' ' ' <"$dir/exit-codes.txt")"
  done
  echo
  cat "$OUT/criteria.txt"
} >"$OUT/summary.txt"
cat "$OUT/summary.txt"
echo "report=$OUT"
exit "$code"
