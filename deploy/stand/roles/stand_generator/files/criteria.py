#!/usr/bin/env python3
"""Отчёт по критериям приёмки APP-D Д.7 для профиля SMB (DEP-13, DEP-14, DEP-25) из журналов прогона run-scenario.sh.

Профиль порогов узла (params.txt: profile, решение 54): speed — рабочие пороги, критерии Д.7 как есть;
thresholds — пороги Д.5, критерий прогона — «узел не падает, отказы только 4xx, принятые письма не потеряны»
(строка «Пороги Д.5»), строки скорости — справочно.

  criteria.py <каталог прогона>                      — criteria.md и criteria.json; код выхода 1, если есть «не прошёл»;
  criteria.py <каталог> --window <t0> <t1> --check   — проверка ступени stress: задержки зондов и доля отказов в окне
                                                       (вывод — нарушения; код 1 — есть нарушение).

Входы каталога: params.txt, events.txt, probes.jsonl (probes.py), probe-k6.jsonl и probe-idle-k6.jsonl (probe.js),
smb-samples.csv (smb-sampler.py, узел SMB; метрики Д.7 узла APP-34 — пул СУБД, NOTIFY, слушатели, «сироты»),
ledger.txt (сверка принятых писем), brute.json (сценарий bruteforce), журналы нагрузки
(smtp-in.log, submission.log, k6-summary.json — в каталоге или в step-*/ для stress).
"""

import csv
import glob
import json
import math
import os
import sys

PASS, FAIL, NONE, NA = "прошёл", "не прошёл", "не измерялось", "не относится к SMB"
PROFILES = {"speed": "рабочие пороги, критерии Д.7", "thresholds": "пороги Д.5, критерий — устойчивость"}
INFO = "справочно"  # профиль thresholds: значение строки скорости, в итог прогона не входит
NO_D7 = "в /metrics узла нет метрик Д.7 (tp_db_*, tp_blob_orphans_*: APP-34) — выпуск узла раньше"
LOSS_GRACE = 60  # с после события отказа: задержки зондов в критерии строк 1–5 не входят (faults)


def pct(values, p):
    v = sorted(x for x in values if x is not None)
    if not v:
        return None
    return v[max(0, math.ceil(p / 100 * len(v)) - 1)]


def fmt(ms):
    if ms is None:
        return "—"
    return f"{ms / 1000:.2f} с" if ms >= 1000 else f"{ms:.0f} мс"


def num(x, unit, div=1):
    return "—" if x is None else f"{x / div:.0f}{unit}"


def kv(path):
    out = {}
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for token in f.read().split():
                k, sep, v = token.partition("=")
                if sep:
                    out[k] = v
    return out


def jsonl(path):
    out = []
    if os.path.exists(path):
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if line.startswith("{"):
                    try:
                        out.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
    return out


def events(path):
    out = []
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for line in f:
                v = line.split()
                if len(v) >= 2:
                    out.append((float(v[0]), v[1], dict(x.split("=", 1) for x in v[2:] if "=" in x)))
    return out


def samples(path):
    out = []
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            for r in csv.DictReader(f):
                out.append({k: (float(v) if v not in ("", None) else None) for k, v in r.items()})
    return out


class Run:
    def __init__(self, d):
        self.d = d
        self.params = kv(os.path.join(d, "params.txt"))
        self.scenario = self.params.get("scenario", "base")
        self.profile = self.params.get("profile", "speed") or "speed"
        self.bus_kind = self.params.get("bus_kind", "")
        self.users = int(self.params.get("users", "0") or 0)
        self.ev = events(os.path.join(d, "events.txt"))
        self.probes = jsonl(os.path.join(d, "probes.jsonl")) + jsonl(os.path.join(d, "probe-k6.jsonl"))
        self.idle_push = jsonl(os.path.join(d, "probe-idle-k6.jsonl"))
        self.samples = samples(os.path.join(d, "smb-samples.csv"))
        self.ledger = kv(os.path.join(d, "ledger.txt"))
        self.loads = sorted({os.path.dirname(p) for p in glob.glob(os.path.join(d, "**", "load.txt"), recursive=True)})
        self.brute = None
        p = os.path.join(d, "brute.json")
        if os.path.exists(p):
            with open(p, encoding="utf-8") as f:
                self.brute = json.load(f)

    def d7(self):
        """Метрики Д.7 узла (APP-34) есть в срезах."""
        return bool(self.node("pool_max"))

    def delta(self, col, t0=None, t1=None):
        """Прирост счётчика узла за окно (сбросы при перезапуске узла не вычитаются)."""
        v = self.node(col, t0, t1)
        return sum(max(0.0, b - a) for a, b in zip(v, v[1:])) if len(v) >= 2 else None

    def when(self, name):
        return [e for e in self.ev if e[1] == name]

    def first(self, name):
        e = self.when(name)
        return e[0][0] if e else None

    def faults(self):
        return [e for e in self.ev if e[1] in ("kill-node", "restart-postgresql")]

    def values(self, probe, t0=None, t1=None, skip_faults=False):
        """Задержки (мс) и число отказов зонда в окне."""
        gaps = [(t, t + LOSS_GRACE) for t, _, _ in self.faults()] if skip_faults else []
        ok, bad = [], 0
        for r in self.probes:
            if r.get("probe") != probe:
                continue
            t = r.get("t", 0)
            if (t0 is not None and t < t0) or (t1 is not None and t > t1):
                continue
            if any(a <= t <= b for a, b in gaps):
                continue
            if r.get("ok") and r.get("ms") is not None:
                ok.append(r["ms"])
            elif not r.get("ok"):
                bad += 1
        return ok, bad

    def node(self, col, t0=None, t1=None):
        return [s[col] for s in self.samples if s.get(col) is not None
                and (t0 is None or s["t"] >= t0) and (t1 is None or s["t"] <= t1)]

    def load_counts(self, t0=None, t1=None):
        """ok и отказы нагрузки SMTP (по кодам) во всех каталогах нагрузки (окно — по ступеням stress)."""
        res = {"ok": 0, "fail": 0, "codes": {}}
        for d in self.loads:
            if t0 is not None:
                st = kv(os.path.join(d, "load.txt"))
                if not (t0 - 1 <= float(st.get("start", 0)) <= t1 + 1):
                    continue
            for name in ("smtp-in.log", "submission.log"):
                p = os.path.join(d, name)
                if not os.path.exists(p):
                    continue
                with open(p, encoding="utf-8") as f:
                    for line in f:
                        v = line.split()
                        if not v:
                            continue
                        if v[0] == "ok":
                            res["ok"] += 1
                        else:
                            res["fail"] += 1
                            code = v[1] if len(v) > 1 else "conn"
                            res["codes"][code] = res["codes"].get(code, 0) + 1
        return res

    def k6_5xx(self):
        n = 0
        for d in self.loads:
            p = os.path.join(d, "k6-summary.json")
            if os.path.exists(p):
                with open(p, encoding="utf-8") as f:
                    n += int(json.load(f).get("metrics", {}).get("http_5xx", {}).get("count", 0))
        return n


def latency_rows(run, t0=None, t1=None, skip_faults=False):
    """Строки Д.7 с задержками зондов: (показатель, порог, значение, статус, комментарий)."""
    def v(p):
        return run.values(p, t0, t1, skip_faults)

    rows = []

    def row(name, crit, probes, p, limit, note=""):
        vals, bad = [], 0
        parts = []
        for pr in probes:
            ok, b = v(pr)
            vals.append(pct(ok, p))
            bad += b
            parts.append(f"{pr}: p{p}={fmt(pct(ok, p))} (n={len(ok)}, отказов {b})")
        if any(x is None for x in vals):
            # Нет ни одного удачного замера: отказы зонда — «не прошёл», иначе зонд не работал.
            status, why = (FAIL, "нет удачных замеров зонда (отказы)") if bad else (NONE, "нет замеров зонда")
            rows.append((name, crit, "; ".join(parts), status, why + (f"; {note}" if note else "")))
        else:
            good = all(x < limit for x in vals) and bad == 0
            rows.append((name, crit, "; ".join(parts), PASS if good else FAIL, note))

    row("JMAP Email/query, Email/get, p95", "< 200 мс", ["jmap_query", "jmap_get"], 95, 200)
    sel = [r.get("exists") for r in run.probes if r.get("probe") == "select" and r.get("exists") is not None]
    exists = max(sel) if sel else None
    if exists is not None and exists < 10000:
        ok, _ = v("select")
        rows.append(("IMAP SELECT папки 10 тыс. писем, p95", "< 100 мс", f"select: p95={fmt(pct(ok, 95))}", NONE,
                     f"в ящике big {exists} писем < 10 000 — наполнение --tags fill"))
    else:
        row("IMAP SELECT папки 10 тыс. писем, p95", "< 100 мс", ["select"], 95, 100,
            f"ящик big: {exists} писем" if exists is not None else "")
    row("Приём SMTP до ответа 250, p95", "< 500 мс", ["smtp"], 95, 500)
    row("Внутренняя доставка до появления в ящике, p95", "< 5 с", ["delivery"], 95, 5000,
        "587 → EXISTS в IMAP IDLE получателя")
    row("Задержка сигнала (фиксация → IMAP IDLE / JMAP push), p99", "< 1 с", ["signal_imap", "signal_push"], 99, 1000,
        "SMB — один узел: сигнал в пределах узла; push — верхняя оценка (с временем Email/set)")
    return rows


def search_rows(run, t0=None, t1=None, skip_faults=False):
    ok, bad = run.values("fts", t0, t1, skip_faults)
    sel = [r.get("exists") for r in run.probes if r.get("probe") == "select" and r.get("exists") is not None]
    size = max(sel) if sel else None
    p95 = pct(ok, 95)
    if p95 is None:
        fts = ("Полнотекстовый поиск, ящик 100 тыс. писем, p95", "< 1 с", f"fts: отказов {bad}",
               FAIL if bad else NONE, "все замеры зонда fts — отказы" if bad else "нет замеров зонда fts")
    else:
        fts = ("Полнотекстовый поиск, ящик 100 тыс. писем, p95", "< 1 с", f"fts: p95={fmt(p95)} (n={len(ok)}, "
               f"отказов {bad})", PASS if p95 < 1000 and bad == 0 else FAIL,
               f"наибольший ящик SMB — big, {size} писем; ящиков 100 тыс. в наполнении SMB (Д.3) нет")
    iok, ibad = run.values("index", t0, t1, skip_faults)
    ip95 = pct(iok, 95)
    idx = ("Задержка индексации, p95", "< 60 с",
           f"index: p95={fmt(ip95)} (n={len(iok)}, не найдено {ibad})" if ip95 is not None else "—",
           (FAIL if ibad else NONE) if ip95 is None else (PASS if ip95 < 60000 and ibad == 0 else FAIL),
           "письмо с уникальным словом на 25 → находится Email/query text")
    return fts, idx


def window(run, start, end):
    return [s for s in run.samples if start <= s["t"] <= end and s.get("rss_bytes") is not None]


def median(v):
    v = sorted(v)
    return v[len(v) // 2] if v else None


def idle_row(run):
    """Прирост RSS узла от первой половины простаивающих соединений ко второй / число добавленных."""
    parts, status = [], []

    def rss(t0, t1):
        return median([s["rss_bytes"] for s in window(run, t0, t1)])

    h, r, c = run.when("idle_imap_half"), run.when("idle_imap_ready"), run.first("idle_imap_close")
    if h and r and c and run.samples:
        added = int(r[0][2].get("added", 0))
        one, two = rss(h[0][0] + 5, r[0][0] - 1), rss(r[0][0] + 5, c - 1)
        if added and one and two:
            kb = (two - one) / added / 1024
            parts.append(f"IMAP IDLE: {kb:.1f} KB (+{added} соединений, RSS {one / 2**20:.0f} → {two / 2**20:.0f} MB)")
            status.append(kb <= 64)
    c = run.first("idle_push_close")
    a_open = [x["t"] for x in run.idle_push if x.get("probe") == "push_open" and x.get("ok") and x.get("half") == "a"]
    b_open = [x["t"] for x in run.idle_push if x.get("probe") == "push_open" and x.get("ok") and x.get("half") == "b"]
    if c and a_open and b_open and run.samples:
        one, two = rss(max(a_open) + 5, min(b_open) - 1), rss(max(b_open) + 5, c - 1)
        if one and two:
            kb = (two - one) / len(b_open) / 1024
            parts.append(f"JMAP push: {kb:.1f} KB (+{len(b_open)} соединений, RSS {one / 2**20:.0f} → "
                         f"{two / 2**20:.0f} MB)")
            status.append(kb <= 64)
    name, crit = "Память на простаивающее соединение IMAP IDLE или JMAP push", "≤ 64 KB"
    if not status:
        why = ("нет срезов узла (smb-samples.csv)" if not run.samples
               else "замер не выполнялся или окно короче среза (stand_idle_connections, stand_idle_hold)")
        return (name, crit, "—", NONE, why)
    return (name, crit, "; ".join(parts), PASS if all(status) else FAIL,
            "прирост RSS узла (/metrics) от N/2 ко N соединениям / (N/2): разовые выделения не входят")


def lost(run):
    if "lost" not in run.ledger:
        return None, "нет сверки (ledger.txt)"
    lg = run.ledger
    n = int(lg["lost"])
    text = (f"принято 250: {lg.get('accepted')}, в ящиках +{lg.get('mailboxes')}, в smtp-sink +{lg.get('sink')}, "
            f"потеряно {max(n, 0)}")
    if n < 0:
        text += (f" (доставлено на {-n} больше принятого: хвост очереди прошлых прогонов, уведомления DSN или 250 "
                 "после тайм-аута клиента)")
    return n, text


def restarts(run):
    pids = {s["pid"] for s in run.samples if s.get("pid")}
    nr = [s["nrestarts"] for s in run.samples if s.get("nrestarts") is not None]
    return (max(nr) - min(nr) if nr else None), len(pids)


def outages(run):
    """Перерывы после событий отказа: от события до первого успешного входа после отказа (name, t, с, восстановлен).

    Без успешного входа до конца зондов — перерыв не меньше, чем до последнего замера (восстановлен = False).
    """
    out = []
    for t, name, _ in run.faults():
        worst, back_all = 0.0, True
        for probe in ("avail_imap", "avail_jmap"):
            recs = sorted((r["t"], r.get("ok")) for r in run.probes if r.get("probe") == probe and r["t"] >= t)
            failed_at = next((x for x, ok in recs if not ok and x <= t + 120), None)
            if failed_at is None:
                continue
            back = next((x for x, ok in recs if ok and x > failed_at), None)
            if back is None:
                back_all = False
                back = recs[-1][0]
            worst = max(worst, back - t)
        out.append((name, t, worst, back_all))
    return out


def outage_text(w, back):
    return f"{w:.1f} с" if back else f"без переподключения до конца замеров (> {w:.0f} с)"


def faults_row(run):
    name, crit = "Отказ узла приложения", "Переподключение клиентов ≤ 30 с, без потери принятых писем"
    if run.scenario != "faults":
        return (name, crit, "—", NONE, "сценарий faults (-e stand_scenario=faults)")
    o = outages(run)
    n_lost, note = lost(run)
    if not o:
        return (name, crit, "—", NONE, "нет событий отказа в events.txt")
    text = "; ".join(f"{n}: {outage_text(w, b)}" for n, _, w, b in o)
    good = all(b and w <= 30 for _, _, w, b in o) and n_lost <= 0
    return (name, crit, text, PASS if good else FAIL, note)


STABLE = "Узел не завершается аварийно; принятые письма не теряются; отказы — только 4xx"


def stability(run, name, crit):
    """Строка «не падает, отказы только 4xx, принятые письма не потеряны» (x3, профиль thresholds).

    В сценарии faults перезапуски узла — внесённый отказ: в критерий не входят.
    """
    nr, pids = restarts(run)
    lc = run.load_counts()
    bad = {c: n for c, n in lc["codes"].items() if not c.startswith("4")}
    h5 = run.k6_5xx()
    n_lost, note = lost(run)
    text = (f"рестартов {nr if nr is not None else '—'} (PID: {pids}); отказы SMTP {lc['fail']} из {lc['ok'] + lc['fail']}"
            f" {lc['codes'] or ''}; JMAP 5xx {h5}; потеряно {max(n_lost, 0) if n_lost is not None else '—'}")
    if nr is None or n_lost is None:
        return (name, crit, text, NONE, "нет срезов узла или сверки")
    fault = run.scenario == "faults"
    good = (fault or (nr == 0 and pids <= 1)) and not bad and h5 == 0 and n_lost <= 0
    if fault:
        note = "перезапуски — внесённые отказы сценария faults" + (f"; {note}" if note else "")
    return (name, crit, text, PASS if good else FAIL, note)


def x3_row(run):
    name = "Поведение под нагрузкой ×3 к номинальной"
    if run.scenario != "x3":
        return (name, STABLE, "—", NONE, "сценарий x3 (-e stand_scenario=x3)")
    return stability(run, name, STABLE)


def thresholds_row(run):
    name = "Пороги Д.5: устойчивость (решение 54)"
    if run.profile != "thresholds":
        return (name, STABLE, "—", NONE, "профиль thresholds (-e stand_profile=thresholds)")
    return stability(run, name, STABLE)


def first_line(path, default):
    if os.path.exists(path):
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.strip():
                    return line.strip()
    return default


def brute_row(run):
    """Защита от перебора со второго адреса (Д.6, решение 56): итог probes.py brute."""
    name = "Защита от перебора (сценарий bruteforce, второй адрес)"
    b = run.brute or {}
    crit = (f"Блокировка учётной записи после {b.get('account_failures', 'account_failures')} и адреса после "
            f"{b.get('ip_failures', 'ip_failures')} неудач (auth.abuse); генератор не блокируется")
    if run.scenario != "bruteforce":
        return (name, crit, "—", NONE,
                "сценарий bruteforce (-e stand_scenario=bruteforce -e stand_bruteforce_source=…)")
    if not run.brute:
        return (name, crit, "—", NONE, first_line(os.path.join(run.d, "brute.log"), "нет brute.json"))
    if b.get("skipped"):
        return (name, crit, "—", NONE, b["skipped"])
    fails = {}
    for x in b.get("attempts", []):
        fails[x["phase"]] = fails.get(x["phase"], 0) + (x["result"] == "no")
    text = "; ".join(f"{x['name']}: {x['result']} (ожидалось {x['expect']})" for x in b.get("checks", []))
    note = (f"со второго адреса {b.get('source')}: отказов входа {b.get('target')} — {fails.get('account', 0)}, "
            f"под несуществующими именами — {fails.get('address', 0)}")
    wrong = [x for x in b.get("checks", []) if x["result"] in ("ok", "no") and not x["ok"]]
    if b.get("inconclusive") and not wrong:
        return (name, crit, text, NONE, "узел не ответил «ok» или «no» (UNAVAILABLE, обрыв — перегрузка): "
                + ", ".join(b["inconclusive"]) + "; " + note)
    return (name, crit, text, PASS if b.get("ok") else FAIL, note)


def long_windows(run):
    t0, t1 = run.first("load_start"), run.first("load_end")
    w = int(run.params.get("long_window", "1800") or 1800)
    out = []
    if t0 is None or t1 is None:
        return out
    t = t0
    while t + w <= t1 + 1:
        rss = [s["rss_bytes"] for s in window(run, t, t + w)]
        out.append({
            "start": t, "rss": median(rss),
            "smtp": pct(run.values("smtp", t, t + w)[0], 95),
            "jmap_query": pct(run.values("jmap_query", t, t + w)[0], 95),
            "select": pct(run.values("select", t, t + w)[0], 95),
        })
        t += w
    return out


def long_row(run):
    name, crit = "Длительный тест 72 ч", "Рост памяти ≤ 10 %, деградация p95 ≤ 20 %"
    if run.scenario != "long":
        return (name, crit, "—", NONE, "сценарий long (-e stand_scenario=long)"), []
    ws = long_windows(run)
    if len(ws) < 2 or not ws[0]["rss"] or not ws[-1]["rss"]:
        return (name, crit, "—", NONE, "меньше двух окон или нет срезов узла"), ws
    growth = (ws[-1]["rss"] - ws[0]["rss"]) / ws[0]["rss"] * 100
    deg = {}
    for k in ("smtp", "jmap_query", "select"):
        if ws[0][k] and ws[-1][k]:
            deg[k] = (ws[-1][k] - ws[0][k]) / ws[0][k] * 100
    hours = (ws[-1]["start"] + int(run.params.get("long_window", 1800)) - ws[0]["start"]) / 3600
    text = f"рост RSS {growth:+.1f} %; деградация p95: " + ", ".join(f"{k} {v:+.0f} %" for k, v in deg.items())
    good = growth <= 10 and all(v <= 20 for v in deg.values())
    note = f"окон {len(ws)} × {run.params.get('long_window')} с, всего {hours:.2f} ч"
    if hours < 72:
        note += " (короче 72 ч Д.6)"
    return (name, crit, text, PASS if good else FAIL, note), ws


def in_process_bus(run):
    return run.bus_kind != "postgres"


def bus_note(run):
    return (f"SMB: шина сигналов в процессе (bus.kind: {run.bus_kind or 'по умолчанию'}) — NOTIFY и слушателей "
            "LISTEN нет")


def notify_row(run):
    """Доля времени фиксаций с NOTIFY во времени всех фиксаций за прогон (META-53, APP-34)."""
    name, crit = "Доля времени фиксации на primary, занятая NOTIFY", "≤ 5 %"
    dn, dc = run.delta("notify_commit_sum"), run.delta("commit_sum")
    if dn is not None and dc is not None:
        if dc <= 0:
            return (name, crit, "фиксаций за прогон: 0", NONE, "tp_db_commit_seconds не рос")
        share = 100 * dn / dc
        n = run.delta("notify_commit_count") or 0
        return (name, crit, f"{share:.2f} % (фиксаций с NOTIFY {n:.0f}, время {dn:.1f} с из {dc:.1f} с)",
                PASS if share <= 5 else FAIL, "прирост tp_db_notify_commit_seconds_sum / tp_db_commit_seconds_sum")
    if not run.d7():
        return (name, crit, "—", NONE, NO_D7)
    if in_process_bus(run):
        return (name, crit, "—", NA, bus_note(run))
    return (name, crit, "—", NONE, "в /metrics узла нет tp_db_notify_commit_seconds и tp_db_commit_seconds")


def listeners_row(run):
    name, crit = "Число соединений-слушателей сигналов (узлы × шарды) на целевом масштабе", "≤ 2 000"
    m = run.node("listeners")
    if m:
        return (name, crit, f"tp_db_listeners: max {max(m):.0f}", PASS if max(m) <= 2000 else FAIL,
                "SMB: 1 узел × 1 шард")
    if run.d7() and in_process_bus(run):
        return (name, crit, "—", NA, bus_note(run))
    v = run.node("pg_listen")
    if not v:
        return (name, crit, "—", NONE, "нет срезов узла (pg_stat_activity)")
    if max(v) == 0:
        return (name, crit, "LISTEN в pg_stat_activity: 0", NONE,
                "слушатель сигналов не виден в pg_stat_activity (последний запрос соединения — не LISTEN)")
    return (name, crit, f"LISTEN в pg_stat_activity: max {max(v):.0f}", PASS if max(v) <= 2000 else FAIL,
            "SMB: 1 узел × 1 шард")


def orphans_row(run):
    """«Сироты» после цикла GC (BLOB-16, APP-34): проходы поиска за прогон и остаток после последнего."""
    name, crit = "«Сироты» после цикла GC", "0"
    rem = run.node("orphans_remaining")
    if not rem:
        return (name, crit, "—", NONE, NO_D7 if not run.d7() else "в /metrics узла нет tp_blob_orphans_remaining")
    passes = run.delta("orphans_passes") or 0
    total = max(run.node("orphans_passes") or [0])
    found, deleted = run.delta("orphans_found") or 0, run.delta("orphans_deleted") or 0
    text = (f"проходов за прогон {passes:.0f} (всего {total:.0f}), найдено {found:.0f}, удалено {deleted:.0f}, "
            f"осталось {rem[-1]:.0f}")
    if total <= 0:
        return (name, crit, text, NONE, "поиск «сирот» ни разу не прошёл (blob.orphans.interval; в профиле "
                "thresholds — 2 мин)")
    return (name, crit, text, PASS if rem[-1] == 0 else FAIL, "tp_blob_orphans_remaining в конце прогона")


def pool_text(run, t0=None, t1=None):
    """Пул СУБД узла (PLT-25, APP-34): занято / предел, ожидающие, среднее ожидание соединения."""
    use, top = run.node("pool_in_use", t0, t1), run.node("pool_max", t0, t1)
    if not use or not top:
        return None
    wait, n = run.delta("pool_wait_sum", t0, t1), run.delta("pool_wait_count", t0, t1)
    waiting = run.node("pool_waiting", t0, t1)
    mean = f", среднее ожидание {1000 * wait / n:.0f} мс (n={n:.0f})" if wait is not None and n else ""
    return f"занято max {max(use):.0f} из {max(top):.0f}, ожидают max {max(waiting or [0]):.0f}{mean}"


def stress_steps(run):
    starts = run.when("step_start")
    ends = {e[2].get("factor"): e[0] for e in run.when("step_end")}
    out = []
    for t0, _, kw in starts:
        f = kw.get("factor")
        t1 = ends.get(f)
        if t1 is None:
            continue
        viol = check(run, t0, t1)
        cpu = run.node("host_cpu_pct", t0, t1)
        pcpu = run.node("proc_cpu_pct", t0, t1)
        rss = run.node("rss_bytes", t0, t1)
        avail = run.node("mem_avail_bytes", t0, t1)
        total = run.node("mem_total_bytes", t0, t1)
        disk = run.node("disk_util_pct", t0, t1)
        pg = run.node("pg_conns", t0, t1)
        use, top = run.node("pool_in_use", t0, t1), run.node("pool_max", t0, t1)
        out.append({
            "factor": f, "t0": t0, "t1": t1, "violations": viol,
            "smtp": pct(run.values("smtp", t0, t1)[0], 95), "jmap_query": pct(run.values("jmap_query", t0, t1)[0], 95),
            "select": pct(run.values("select", t0, t1)[0], 95), "delivery": pct(run.values("delivery", t0, t1)[0], 95),
            "cpu": sum(cpu) / len(cpu) if cpu else None, "proc_cpu": sum(pcpu) / len(pcpu) if pcpu else None,
            "rss": max(rss) if rss else None,
            "mem_used_pct": (100 * (1 - min(avail) / max(total))) if avail and total else None,
            "disk": sum(disk) / len(disk) if disk else None, "pg": max(pg) if pg else None,
            "pool_use": max(use) if use else None, "pool_max": max(top) if top else None,
            "pool": pool_text(run, t0, t1),
        })
    return out


def bottleneck(step, pool):
    """Узкое место ступени: ресурс узла, ближайший к пределу."""
    cand = []
    if step["cpu"] is not None:
        cand.append((step["cpu"] / 85, f"CPU узла {step['cpu']:.0f} %"))
    if step["mem_used_pct"] is not None:
        cand.append((step["mem_used_pct"] / 90, f"память {step['mem_used_pct']:.0f} %"))
    if step["disk"] is not None:
        cand.append((step["disk"] / 80, f"диск {step['disk']:.0f} %"))
    if step.get("pool_use") is not None and step.get("pool_max"):
        cand.append((step["pool_use"] / step["pool_max"], f"пул СУБД {step['pool']}"))
    elif step["pg"] is not None and pool:
        cand.append((step["pg"] / pool, f"пул СУБД {step['pg']:.0f} из {pool} (pg_stat_activity)"))
    if not cand:
        return "нет срезов узла"
    score, text = max(cand)
    return text + ("" if score >= 0.9 else " (ни один ресурс узла не у предела: вероятно, генератор или задержки узла)")


def check(run, t0, t1):
    """Нарушения Д.7 в окне (ступень stress)."""
    viol = [f"{r[0]}: {r[2]}" for r in latency_rows(run, t0, t1) + list(search_rows(run, t0, t1)) if r[3] == FAIL]
    lc = run.load_counts(t0, t1)
    total = lc["ok"] + lc["fail"]
    if total and lc["fail"] / total > 0.01:
        viol.append(f"отказы нагрузки SMTP {lc['fail']} из {total} {lc['codes']}")
    return viol


def report(run):
    skip = run.scenario == "faults"
    t0 = t1 = None
    scope = "весь прогон"
    steps = []
    if run.scenario == "stress":
        steps = stress_steps(run)
        if steps:
            t0, t1, scope = steps[0]["t0"], steps[0]["t1"], f"ступень ×{steps[0]['factor']}"
    elif skip:
        scope = f"весь прогон без {LOSS_GRACE} с после каждого отказа"
    lat = latency_rows(run, t0, t1, skip)
    fts, idx = search_rows(run, t0, t1, skip)
    long, ws = long_row(run)
    users_note = (f"уменьшенный стенд: users={run.users}" if run.users < 1000 else "")
    rows = [
        lat[0], lat[1], lat[2], lat[3], lat[4],
        notify_row(run),
        listeners_row(run),
        fts,
        ("Полнотекстовый поиск, общий ящик 1 млн писем, p95", "< 3 с", "—", NA, "общих ящиков 1 млн в SMB нет (Д.3)"),
        idx,
        ("Холодная загрузка индекса ящика 100 тыс. писем", "< 10 с", "—", NONE,
         "ящиков 100 тыс. в SMB нет; сброса кэша индекса без перезапуска узла нет"),
        ("Переключение primary PostgreSQL", "RTO ≤ 30 с, RPO = 0", "—", NA,
         "один PostgreSQL без HA; перезапуск PostgreSQL — в сценарии faults"),
        faults_row(run),
        orphans_row(run),
        ("Потерянные объекты после PITR (thepost verify)", "0", "—", NONE, "сценарий «Восстановление» стендом не выполняется"),
        long,
        idle_row(run),
        x3_row(run),
    ]
    if run.profile == "thresholds":
        # Решение 54: на порогах Д.5 скорость не судится — значения справочно; судятся строка «Пороги Д.5»,
        # «сироты» (поиск раз в 2 мин — только на этих порогах) и ×3 (тот же критерий устойчивости).
        judged = {orphans_row(run)[0], x3_row(run)[0]}
        rows = [(r[0], r[1], r[2], INFO, "профиль thresholds: пороги Д.5, критерий скорости не применяется"
                 + (f"; {r[4]}" if r[4] else "")) if r[3] in (PASS, FAIL) and r[0] not in judged else r for r in rows]
    rows += [thresholds_row(run), brute_row(run)]
    measured = [r for r in rows if r[3] in (PASS, FAIL)]
    failed = [r for r in rows if r[3] == FAIL]
    mem = run.node("mem_total_bytes")
    node = f"узел: память {max(mem) / 2**30:.1f} GB" if mem else "узел: нет срезов"
    if run.users < 1000:
        rows.append(("SMB, 1 000 пользователей", "Все критерии на 4 vCPU / 8 GB", f"users={run.users}; {node}", NONE,
                     users_note))
    else:
        rows.append(("SMB, 1 000 пользователей", "Все критерии на 4 vCPU / 8 GB",
                     f"измерено {len(measured)}, не прошло {len(failed)}; {node}", FAIL if failed else PASS, ""))

    lc = run.load_counts()
    n_lost, ledger_note = lost(run)
    lines = [
        f"# Критерии Д.7, профиль SMB — прогон {run.params.get('run', os.path.basename(run.d))}",
        "",
        f"Профиль `{run.profile}` ({PROFILES.get(run.profile, '')}), "
        f"сценарий `{run.scenario}`, множитель {run.params.get('factor')}, длительность "
        f"{run.params.get('duration')}, пользователей {run.users}; задержки — {scope}.",
        "",
        "| # | Показатель | Порог | Значение | Итог | Комментарий |",
        "|---|---|---|---|---|---|",
    ]
    for i, r in enumerate(rows, 1):
        lines.append(f"| {i} | {r[0]} | {r[1]} | {r[2]} | **{r[3]}** | {r[4]} |")
    lines += ["", "## Нагрузка и сверка писем", "",
              f"- SMTP нагрузки (25 и 587): принято {lc['ok']}, отказов {lc['fail']} {lc['codes'] or ''}",
              f"- {ledger_note}",
              f"- Пул СУБД узла: {pool_text(run) or 'нет tp_db_pool_* в /metrics узла (APP-34)'}"]
    if steps:
        pool = run.params.get("db_pool")
        pool = int(pool) if pool and pool.isdigit() else None
        sat = None
        lines += ["", "## Стресс: ступени", "",
                  "| Множитель | SMTP p95 | JMAP query p95 | SELECT p95 | доставка p95 | CPU узла | CPU thepost | "
                  "RSS | память | диск | пул (соединений) СУБД | нарушения |",
                  "|---|---|---|---|---|---|---|---|---|---|---|---|"]
        for s in steps:
            lines.append(
                f"| ×{s['factor']} | {fmt(s['smtp'])} | {fmt(s['jmap_query'])} | {fmt(s['select'])} | "
                f"{fmt(s['delivery'])} | {num(s['cpu'], ' %')} | {num(s['proc_cpu'], ' %')} | "
                f"{num(s['rss'], ' MB', 2**20)} | {num(s['mem_used_pct'], ' %')} | {num(s['disk'], ' %')} | "
                f"{s['pool'] or num(s['pg'], '')} | {'; '.join(s['violations']) or '—'} |")
            if s["violations"] and sat is None:
                sat = s
        ok = [s for s in steps if not s["violations"]]
        if sat:
            point = f"×{ok[-1]['factor']}" if ok else "нет — нарушение уже на первой ступени"
            lines += ["", f"Точка насыщения: {point}; первое нарушение — ×{sat['factor']}. "
                      f"Узкое место: {bottleneck(sat, pool)}."]
        else:
            lines += ["", f"Нарушений нет до ×{steps[-1]['factor']} (последняя ступень stand_stress_factors). "
                      f"Наиболее загружено: {bottleneck(steps[-1], pool)}."]
    if ws:
        lines += ["", "## Длительный прогон: окна", "", "| Начало окна | RSS | SMTP p95 | JMAP query p95 | SELECT p95 |",
                  "|---|---|---|---|---|"]
        for w in ws:
            lines.append(f"| +{(w['start'] - ws[0]['start']) / 60:.0f} мин | {num(w['rss'], ' MB', 2**20)} | "
                         f"{fmt(w['smtp'])} | {fmt(w['jmap_query'])} | {fmt(w['select'])} |")
    if run.scenario == "faults":
        lines += ["", "## Отказы", ""]
        for name, t, w, b in outages(run):
            lines.append(f"- {name}: перерыв {outage_text(w, b)}")
    lines.append("")
    data = {
        "run": run.params.get("run"), "scenario": run.scenario, "users": run.users,
        "profile": run.profile,
        "rows": [dict(zip(("name", "criterion", "value", "status", "note"), r)) for r in rows],
        "load": lc, "ledger": run.ledger, "lost": n_lost, "stress": steps, "long": ws,
        "failed": len(failed),
    }
    return "\n".join(lines), data, bool(failed)


def main():
    d = sys.argv[1]
    run = Run(d)
    if "--check" in sys.argv:
        i = sys.argv.index("--window")
        viol = check(run, float(sys.argv[i + 1]), float(sys.argv[i + 2]))
        for v in viol:
            print(v)
        return 1 if viol else 0
    md, data, failed = report(run)
    with open(os.path.join(d, "criteria.md"), "w", encoding="utf-8") as f:
        f.write(md)
    with open(os.path.join(d, "criteria.json"), "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=1, default=str)
    print(md)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
