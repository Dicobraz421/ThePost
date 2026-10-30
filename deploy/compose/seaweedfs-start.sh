#!/bin/sh
# SeaweedFS в compose: сервер с S3 и бакет для интеграционных тестов при старте.
# /tmp/ready — бакет есть и S3 слушает порт (healthcheck); повторный старт бакет не трогает.
set -eu

BUCKET=${TP_S3_BUCKET:-thepost-test}
rm -f /tmp/ready
weed -logtostderr=true "$@" &
pid=$!
trap 'kill -TERM "$pid"' TERM INT

shell() { echo "$1" | weed shell -master=127.0.0.1:9333 2>/dev/null; }
until shell s3.bucket.list | grep -qw "$BUCKET" || shell "s3.bucket.create -name $BUCKET" | grep -q created; do
  kill -0 "$pid"
  sleep 1
done
echo "seaweedfs: бакет $BUCKET есть"
touch /tmp/ready

# Первый wait прерывает сигнал (trap), второй дожидается остановки сервера.
wait "$pid" || wait "$pid"
