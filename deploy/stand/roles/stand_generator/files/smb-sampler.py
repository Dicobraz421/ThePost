#!/usr/bin/env python3
"""Срезы узла SMB во время прогона стенда (DEP-13, DEP-14, DEP-25): раз в <период> с — строка smb-samples.csv до
появления файла <каталог>/stop. Источники: /metrics служебного HTTP (tp_process_resident_memory_bytes, tp_connections;
метрики Д.7 узла — APP-34: пул СУБД tp_db_pool_*, фиксации с NOTIFY tp_db_notify_commit_seconds и все фиксации
tp_db_commit_seconds, слушатели tp_db_listeners, «сироты» tp_blob_orphans_*; нет в выпуске — пустые столбцы),
/proc (CPU узла и процесса, память, загрузка дисков), systemd (MainPID, NRestarts), PostgreSQL (соединения thepost,
слушатели LISTEN; psql от postgres).

Запуск (root): smb-sampler.py <каталог> <период, с> <адрес служебного HTTP> [предел, с]
"""

import csv
import os
import re
import ssl
import subprocess
import sys
import time
import urllib.request

COLUMNS = [
    "t", "pid", "nrestarts", "rss_bytes", "proc_cpu_pct", "host_cpu_pct", "mem_avail_bytes", "mem_total_bytes",
    "disk_util_pct", "pg_conns", "pg_listen", "connections", "role_restarts",
] + [
    "pool_in_use", "pool_max", "pool_waiting", "pool_wait_sum", "pool_wait_count", "commit_sum", "commit_count",
    "notify_commit_sum", "notify_commit_count", "notify_share", "listeners", "listener_connected", "orphans_passes",
    "orphans_found", "orphans_deleted", "orphans_remaining",
]
# Метрики Д.7 узла (APP-34): столбец → имя в /metrics (счётчики — с суффиксом _total и без; метки суммируются).
D7 = {
    "pool_in_use": "tp_db_pool_in_use", "pool_max": "tp_db_pool_max", "pool_waiting": "tp_db_pool_waiting",
    "pool_wait_sum": "tp_db_pool_wait_seconds_sum", "pool_wait_count": "tp_db_pool_wait_seconds_count",
    "commit_sum": "tp_db_commit_seconds_sum", "commit_count": "tp_db_commit_seconds_count",
    "notify_commit_sum": "tp_db_notify_commit_seconds_sum", "notify_commit_count": "tp_db_notify_commit_seconds_count",
    "notify_share": "tp_db_notify_commit_share", "listeners": "tp_db_listeners",
    "listener_connected": "tp_db_listener_connected", "orphans_passes": "tp_blob_orphans_passes",
    "orphans_found": "tp_blob_orphans_found", "orphans_deleted": "tp_blob_orphans_deleted",
    "orphans_remaining": "tp_blob_orphans_remaining",
}
D7_BY_NAME = {v: k for k, v in D7.items()}
DISK = re.compile(r"^(sd[a-z]+|vd[a-z]+|xvd[a-z]+|nvme\d+n\d+)$")
TICK = os.sysconf("SC_CLK_TCK")
NCPU = os.cpu_count() or 1


def cmd(*argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=10).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def cpu_total():
    with open("/proc/stat") as f:
        v = [int(x) for x in f.readline().split()[1:]]
    return sum(v), v[3] + v[4]


def proc_cpu(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            v = f.read().rsplit(")", 1)[1].split()
        return int(v[11]) + int(v[12])
    except (OSError, IndexError, ValueError):
        return None


def disk_ticks():
    out = {}
    with open("/proc/diskstats") as f:
        for line in f:
            v = line.split()
            if DISK.match(v[2]):
                out[v[2]] = int(v[12])
    return out


def meminfo():
    m = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, v = line.split(":", 1)
            m[k] = int(v.split()[0]) * 1024
    return m.get("MemAvailable", 0), m.get("MemTotal", 0)


def metrics(url, ctx):
    try:
        with urllib.request.urlopen(f"{url}/metrics", context=ctx, timeout=5) as r:
            text = r.read().decode("utf-8", "replace")
    except OSError:
        return None, None, None, {}
    rss = conns = restarts = None
    d7 = {}
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        name, _, value = line.rpartition(" ")
        base = name.split("{", 1)[0]
        try:
            x = float(value)
        except ValueError:
            continue
        if base == "tp_process_resident_memory_bytes":
            rss = x
        elif base == "tp_connections":
            conns = (conns or 0) + x
        elif base in ("tp_role_restarts", "tp_role_restarts_total"):
            restarts = (restarts or 0) + x
        col = D7_BY_NAME.get(base[:-6] if base.endswith("_total") else base)
        if col:
            d7[col] = d7.get(col, 0) + x
    return rss, conns, restarts, d7


def vmrss(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        pass
    return None


def main():
    out, period, url = sys.argv[1], float(sys.argv[2]), sys.argv[3]
    limit = float(sys.argv[4]) if len(sys.argv) > 4 else 300000
    os.makedirs(out, exist_ok=True)
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # loopback узла; сертификат выпущен на имя узла
    pg = "select count(*) filter (where usename like 'thepost%'), count(*) filter (where query ilike 'listen%') " \
         "from pg_stat_activity"
    end = time.time() + limit
    prev = None
    with open(os.path.join(out, "smb-samples.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(COLUMNS)
        while not os.path.exists(os.path.join(out, "stop")) and time.time() < end:
            t0 = time.time()
            show = dict(
                line.split("=", 1) for line in cmd("systemctl", "show", "-p", "MainPID", "-p", "NRestarts", "thepost")
                .splitlines() if "=" in line
            )
            pid = show.get("MainPID", "0")
            total, idle = cpu_total()
            pticks = proc_cpu(pid) if pid != "0" else None
            disks = disk_ticks()
            avail, mem_total = meminfo()
            rss, conns, restarts, d7 = metrics(url, ctx)
            if rss is None and pid != "0":
                rss = vmrss(pid)
            pgv = cmd("runuser", "-u", "postgres", "--", "psql", "-XAtc", pg).split("|")
            row = {
                "t": f"{t0:.3f}", "pid": pid, "nrestarts": show.get("NRestarts", ""), "rss_bytes": rss or "",
                "mem_avail_bytes": avail, "mem_total_bytes": mem_total,
                "pg_conns": pgv[0] if len(pgv) == 2 else "", "pg_listen": pgv[1] if len(pgv) == 2 else "",
                "connections": conns if conns is not None else "", "role_restarts": restarts if restarts is not None else "",
                "host_cpu_pct": "", "proc_cpu_pct": "", "disk_util_pct": "",
            }
            row.update({c: d7.get(c, "") for c in D7})
            if prev is not None:
                dt = t0 - prev["t"]
                dtotal = total - prev["total"]
                if dtotal > 0:
                    row["host_cpu_pct"] = f"{100 * (1 - (idle - prev['idle']) / dtotal):.1f}"
                if pticks is not None and prev["pid"] == pid and prev["pticks"] is not None:
                    row["proc_cpu_pct"] = f"{100 * (pticks - prev['pticks']) / TICK / dt / NCPU:.1f}"
                util = [disks[d] - prev["disks"].get(d, disks[d]) for d in disks]
                if util:
                    row["disk_util_pct"] = f"{min(100.0, 100 * max(util) / (dt * 1000)):.1f}"
            prev = {"t": t0, "total": total, "idle": idle, "pid": pid, "pticks": pticks, "disks": disks}
            w.writerow([row[c] for c in COLUMNS])
            f.flush()
            time.sleep(max(0.0, period - (time.time() - t0)))


if __name__ == "__main__":
    main()
