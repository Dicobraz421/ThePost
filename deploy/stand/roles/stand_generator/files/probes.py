#!/usr/bin/env python3
"""Зонды стенда SMB на время прогона (APP-D Д.7, DEP-13) — Python stdlib (imaplib, smtplib, urllib).

  probes.py run   — до появления файла --stop: smtp (MAIL … DATA → 250 на 25), delivery (587 → EXISTS в IMAP IDLE),
                    signal (STORE флага → FETCH в IDLE другого соединения), select (SELECT INBOX ящика big),
                    index (письмо с уникальным словом на 25 → находится Email/query text), avail (вход IMAP и сессия
                    JMAP раз в период — переподключение при отказах);
  probes.py idle  — N простаивающих соединений IMAP IDLE на --hold с (память на соединение, отметки — в --events);
  probes.py count — писем в ящиках всех учётных записей (IMAP STATUS; без Sent, Drafts, Trash) в JSON (--exclude —
                    кроме этих: цель перебора может быть заблокирована);
  probes.py brute — перебор паролей IMAP со второго адреса (--source, решение 56): неверные пароли учётной записи
                    --target и несуществующие имена; проверки — вход верным паролем со второго адреса и с адреса
                    генератора (итог — --out-json).

Замеры — строки JSON в --out: {"t": unix, "probe": имя, "ms": задержка, "ok": bool, "accepted": писем принято}.
Учётные записи — --accounts (login:пароль): нагрузка — <prefix><n>@<домен>, зонды — probe1 … probe3, ящик big.
"""

import argparse
import base64
import imaplib
import json
import random
import re
import resource
import secrets
import smtplib
import socket
import ssl
import string
import sys
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from email.message import EmailMessage

SKIP_FOLDERS = re.compile(rb"\\(Sent|Drafts|Trash)\b", re.I)


class Ctx:
    def __init__(self, a):
        self.a = a
        self.accounts = {}
        with open(a.accounts, encoding="utf-8") as f:
            for line in f:
                login, _, pw = line.rstrip("\n").partition(":")
                if login:
                    self.accounts[login] = pw
        self.load = [f"{a.prefix}{n}@{a.domain}" for n in range(1, a.users + 1)]
        self.ssl = ssl.create_default_context(cafile=a.ca or None)
        self.lock = threading.Lock()
        self.out = open(a.out, "a", encoding="utf-8") if getattr(a, "out", None) else None
        self.stop = threading.Event()

    def acct(self, name):
        login = name if "@" in name else f"{name}@{self.a.domain}"
        return login, self.accounts[login]

    def log(self, probe, ms=None, ok=True, **kw):
        rec = {"t": round(time.time(), 3), "probe": probe, "ok": ok}
        if ms is not None:
            rec["ms"] = round(ms, 2)
        rec.update(kw)
        with self.lock:
            self.out.write(json.dumps(rec, ensure_ascii=False) + "\n")
            self.out.flush()

    def imap(self, name):
        login, pw = self.acct(name)
        m = imaplib.IMAP4_SSL(self.a.host, 993, ssl_context=self.ssl, timeout=30)
        m.login(login, pw)
        return m

    def message(self, to, subject, body):
        msg = EmailMessage()
        msg["From"] = "probe@external.test"
        msg["To"] = to
        msg["Subject"] = subject
        msg["Message-ID"] = f"<probe-{secrets.token_hex(8)}@external.test>"
        msg.set_content(body + "\n")
        return msg.as_bytes()

    def send25(self, to, data):
        s = smtplib.SMTP(self.a.host, 25, timeout=30)
        try:
            s.ehlo("probe.external.test")
            t0 = time.monotonic()
            s.sendmail("probe@external.test", [to], data)
            return (time.monotonic() - t0) * 1000
        finally:
            close(s)

    def jmap(self, name, calls):
        login, pw = self.acct(name)
        auth = "Basic " + base64.b64encode(f"{login}:{pw}".encode()).decode()
        base = f"https://{self.a.host}"
        if name not in JMAP_ACCOUNTS:
            req = urllib.request.Request(f"{base}/jmap/session", headers={"Authorization": auth})
            with urllib.request.urlopen(req, context=self.ssl, timeout=30) as r:
                JMAP_ACCOUNTS[name] = json.load(r)["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        acc = JMAP_ACCOUNTS[name]
        body = {
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [[m, dict(args, accountId=acc), str(i)] for i, (m, args) in enumerate(calls)],
        }
        req = urllib.request.Request(
            f"{base}/jmap/api",
            data=json.dumps(body).encode(),
            headers={"Authorization": auth, "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, context=self.ssl, timeout=30) as r:
            return json.load(r)["methodResponses"]


JMAP_ACCOUNTS = {}


def close(conn):
    try:
        if isinstance(conn, smtplib.SMTP):
            conn.quit()
        elif isinstance(conn, imaplib.IMAP4):
            conn.logout()
    except (OSError, imaplib.IMAP4.error, smtplib.SMTPException):
        pass


def word():
    return "zq" + "".join(random.choice(string.ascii_lowercase) for _ in range(12))


def idle_begin(m):
    tag = m._new_tag()
    m.send(tag + b" IDLE\r\n")
    line = m.readline()
    if not line.startswith(b"+"):
        raise imaplib.IMAP4.error(f"IDLE: {line!r}")
    return tag


def idle_wait(m, pattern, timeout):
    """Строка ответа IDLE, совпавшая с pattern, или None по истечении timeout."""
    end = time.monotonic() + timeout
    while True:
        left = end - time.monotonic()
        if left <= 0:
            return None
        m.sock.settimeout(left)
        try:
            line = m.readline()
        except (socket.timeout, TimeoutError):
            return None
        finally:
            m.sock.settimeout(30)
        if not line:
            raise imaplib.IMAP4.abort("соединение закрыто в IDLE")
        if re.search(pattern, line):
            return line


def idle_end(m, tag):
    m.send(b"DONE\r\n")
    while True:
        line = m.readline()
        if not line:
            raise imaplib.IMAP4.abort("соединение закрыто после DONE")
        if line.startswith(tag):
            return


def forever(c, name, period, body):
    """Повтор body с периодом period до c.stop; ошибка — запись ok=false и новое соединение."""
    state = {}
    while not c.stop.is_set():
        started = time.monotonic()
        try:
            body(state)
        except Exception as e:  # noqa: BLE001 — зонд не падает: отказ — замер «не прошёл»
            c.log(name, ok=False, err=f"{type(e).__name__}: {e}"[:200])
            for conn in state.values():
                close(conn)
            state.clear()
        c.stop.wait(max(0.0, period - (time.monotonic() - started)))
    for conn in state.values():
        close(conn)


def ensure_message(c, name):
    """В INBOX зонда есть письмо (для изменения флагов)."""
    m = c.imap(name)
    try:
        typ, data = m.select("INBOX")
        if int(data[0]) == 0:
            login, _ = c.acct(name)
            c.send25(login, c.message(login, "probe seed", "seed"))
            c.log("seed", accepted=1)
            for _ in range(60):
                time.sleep(1)
                typ, data = m.select("INBOX")
                if int(data[0]) > 0:
                    break
    finally:
        close(m)


def run(a):
    c = Ctx(a)
    p = a.interval
    big_login, _ = c.acct(a.big)
    probe1, _ = c.acct("probe1")
    probe2, _ = c.acct("probe2")
    ensure_message(c, "probe2")
    ensure_message(c, "probe3")

    def smtp(_):
        to = random.choice(c.load)
        ms = c.send25(to, c.message(to, "probe smtp", "smtp " + word()))
        c.log("smtp", ms, accepted=1)

    def delivery(st):
        if "idle" not in st:
            st["idle"] = c.imap("probe2")
            st["idle"].select("INBOX")
        if "sub" not in st:
            s = smtplib.SMTP(a.host, 587, timeout=30)
            s.starttls(context=c.ssl)
            s.login(*c.acct("probe1"))
            st["sub"] = s
        m = st["idle"]
        tag = idle_begin(m)
        t0 = time.monotonic()
        st["sub"].sendmail(probe1, [probe2], c.message(probe2, "probe delivery", "delivery " + word()))
        line = idle_wait(m, rb"^\* \d+ EXISTS", 60)
        ms = (time.monotonic() - t0) * 1000
        idle_end(m, tag)
        c.log("delivery", ms if line else None, ok=line is not None, accepted=1)

    def signal(st):
        if "a" not in st:
            st["a"] = c.imap("probe2")
            st["a"].select("INBOX")
            st["b"] = c.imap("probe2")
            st["b"].select("INBOX")
            st["on"] = False
        st["on"] = not st["on"]
        b = st["b"]
        tag = idle_begin(b)
        t0 = time.monotonic()
        st["a"].store("1", "+FLAGS" if st["on"] else "-FLAGS", r"(\Flagged)")
        line = idle_wait(b, rb"^\* \d+ FETCH", 30)
        ms = (time.monotonic() - t0) * 1000
        idle_end(b, tag)
        c.log("signal_imap", ms if line else None, ok=line is not None)

    def select(st):
        if "m" not in st:
            st["m"] = c.imap(a.big)
        t0 = time.monotonic()
        typ, data = st["m"].select("INBOX")
        ms = (time.monotonic() - t0) * 1000
        c.log("select", ms, ok=typ == "OK", exists=int(data[0]) if typ == "OK" else None)

    def index(_):
        w = word()
        c.send25(probe1, c.message(probe1, "probe index", f"index {w}"))
        t0 = time.monotonic()
        c.log("index_sent", accepted=1)
        while not c.stop.is_set() and time.monotonic() - t0 < 180:
            r = c.jmap("probe1", [("Email/query", {"filter": {"text": w}, "limit": 1})])
            if r[0][1].get("ids"):
                c.log("index", (time.monotonic() - t0) * 1000)
                return
            time.sleep(1)
        c.log("index", ok=False, err="не найдено за 180 с")

    def avail_imap(_):
        t0 = time.monotonic()
        m = c.imap("probe1")
        m.noop()
        close(m)
        c.log("avail_imap", (time.monotonic() - t0) * 1000)

    def avail_jmap(_):
        login, pw = c.acct("probe1")
        auth = "Basic " + base64.b64encode(f"{login}:{pw}".encode()).decode()
        t0 = time.monotonic()
        req = urllib.request.Request(f"https://{a.host}/jmap/session", headers={"Authorization": auth})
        with urllib.request.urlopen(req, context=c.ssl, timeout=10) as r:
            r.read()
        c.log("avail_jmap", (time.monotonic() - t0) * 1000)

    probes = [
        ("smtp", p, smtp),
        ("delivery", 5 * p, delivery),
        ("signal_imap", p, signal),
        ("select", p, select),
        ("index", 5 * p, index),
        ("avail_imap", 1, avail_imap),
        ("avail_jmap", 1, avail_jmap),
    ]
    c.log("probes_start", big=big_login)
    threads = [threading.Thread(target=forever, args=(c, n, per, b), daemon=True) for n, per, b in probes]
    for t in threads:
        t.start()
    while not c.stop.is_set():
        try:
            with open(a.stop):
                c.stop.set()
        except FileNotFoundError:
            time.sleep(1)
    for t in threads:
        t.join(timeout=240)
    c.log("probes_end")


def idle(a):
    """N соединений IMAP IDLE двумя половинами, каждая держится hold/2 с: отметки idle_imap_* в --events.

    Память на соединение — прирост RSS узла от первой половины ко второй / (N/2): разовые выделения (буферы
    проверки паролей, пулы) в разность не входят.
    """
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (hard, hard))
    c = Ctx(a)

    def opened(i):
        m = c.imap(c.load[i % len(c.load)])
        m.select("INBOX")
        idle_begin(m)
        return m

    def event(name, **kw):
        with open(a.events, "a", encoding="utf-8") as f:
            f.write(f"{time.time():.3f} {name}" + "".join(f" {k}={v}" for k, v in kw.items()) + "\n")

    def batch(first, n):
        got, failed = [], 0
        with ThreadPoolExecutor(25) as ex:
            for f in [ex.submit(opened, first + i) for i in range(n)]:
                try:
                    got.append(f.result())
                except Exception:  # noqa: BLE001 — отказ соединения учитывается в числе
                    failed += 1
        return got, failed

    half = max(1, a.n // 2)
    event("idle_imap_open")
    first, f1 = batch(0, half)
    event("idle_imap_half", n=len(first), failed=f1)
    time.sleep(a.hold / 2)
    second, f2 = batch(half, half)
    event("idle_imap_ready", n=len(first) + len(second), added=len(second), failed=f2)
    time.sleep(a.hold / 2)
    event("idle_imap_close", n=len(first) + len(second))
    for m in first + second:
        try:
            m.shutdown()
        except OSError:
            pass
    time.sleep(5)
    event("idle_imap_closed")


def count(a):
    c = Ctx(a)

    def one(login):
        m = imaplib.IMAP4_SSL(a.host, 993, ssl_context=c.ssl, timeout=60)
        try:
            m.login(login, c.accounts[login])
            typ, boxes = m.list()
            total = 0
            for line in boxes or []:
                if not line or SKIP_FOLDERS.search(line):
                    continue
                name = line.rsplit(b' "', 1)[-1].rstrip(b'"') if line.endswith(b'"') else line.rsplit(b" ", 1)[-1]
                typ, data = m.status('"' + name.decode().replace('"', '\\"') + '"', "(MESSAGES)")
                if typ == "OK":
                    total += int(data[0].decode().rsplit("MESSAGES", 1)[1].strip(" )"))
            return login, total
        finally:
            close(m)

    skip = set(a.exclude)
    with ThreadPoolExecutor(10) as ex:
        per = dict(ex.map(one, [x for x in c.accounts if x not in skip]))
    with open(a.out_json, "w", encoding="utf-8") as f:
        json.dump({"t": time.time(), "total": sum(per.values()), "per": per}, f, sort_keys=True)
    print(sum(per.values()))


class SourceIMAP(imaplib.IMAP4_SSL):
    """IMAP4_SSL с адресом источника (второй адрес генератора)."""

    def __init__(self, host, port, source, **kw):
        self.source = source
        super().__init__(host, port, **kw)

    def _create_socket(self, timeout):
        src = (self.source, 0) if self.source else None
        sock = socket.create_connection((self.host, self.port), timeout, source_address=src)
        return self.ssl_context.wrap_socket(sock, server_hostname=self.host)


def try_login(c, login, pw, source=""):
    """Вход IMAP: ("ok" | "no" | "conn", текст ответа, мс)."""
    t0 = time.monotonic()
    try:
        m = SourceIMAP(c.a.host, 993, source, ssl_context=c.ssl, timeout=30)
    except (OSError, imaplib.IMAP4.error) as e:
        return "conn", str(e)[:200], (time.monotonic() - t0) * 1000
    try:
        m.login(login, pw)
        return "ok", "OK", (time.monotonic() - t0) * 1000
    except imaplib.IMAP4.error as e:
        return "no", str(e)[:200], (time.monotonic() - t0) * 1000
    except OSError as e:
        return "conn", str(e)[:200], (time.monotonic() - t0) * 1000
    finally:
        close(m)


def brute(a):
    """Перебор со второго адреса (Д.6, решение 56): учётная запись, затем адрес.

    1. Исходное состояние: --target входит верным паролем со второго адреса (иначе блокировки прошлого прогона ещё
       действуют — оценка невозможна).
    2. Учётная запись: --account-failures + 1 неверных паролей --target со второго адреса → верный пароль --target не
       пускают ни со второго адреса, ни с адреса генератора (блокировка учётной записи); учётная запись нагрузки
       --check с адреса генератора входит (генератор не заблокирован).
    3. Адрес: --ip-failures + 1 неверных входов под несуществующими именами со второго адреса → верный пароль --check
       со второго адреса не пускают (блокировка адреса), с адреса генератора — пускают.
    """
    c = Ctx(a)
    target, tpw = c.acct(a.target)
    check, cpw = c.acct(a.check)
    res = {"source": a.source, "target": target, "check": check, "ip_failures": a.ip_failures,
           "account_failures": a.account_failures, "attempts": [], "checks": []}

    def attempt(phase, login, pw):
        out, text, ms = try_login(c, login, pw, a.source)
        res["attempts"].append({"t": round(time.time(), 3), "phase": phase, "login": login, "result": out,
                                "text": text, "ms": round(ms, 1)})
        return out

    def verify(name, login, pw, source, expect):
        out, text, ms = try_login(c, login, pw, source)
        res["checks"].append({"t": round(time.time(), 3), "name": name, "login": login, "source": source or "генератор",
                              "expect": expect, "result": out, "text": text, "ms": round(ms, 1),
                              "ok": out == expect})

    out, text, _ = try_login(c, target, tpw, a.source)
    res["baseline"] = {"result": out, "text": text}
    if out != "ok":
        res["skipped"] = (f"исходный вход {target} со второго адреса — {out} ({text}): блокировка прошлого прогона "
                          "(auth.abuse.block_for) или адрес не настроен на генераторе")
    else:
        for _ in range(a.account_failures + 1):
            attempt("account", target, "wrong-" + secrets.token_hex(6))
        verify("учётная запись: верный пароль со второго адреса", target, tpw, a.source, "no")
        verify("учётная запись: верный пароль с адреса генератора", target, tpw, "", "no")
        verify("генератор после блокировки учётной записи", check, cpw, "", "ok")
        for _ in range(a.ip_failures + 1):
            attempt("address", f"nobody-{secrets.token_hex(4)}@{a.domain}", "wrong-" + secrets.token_hex(6))
        verify("адрес: верный пароль другой учётной записи со второго адреса", check, cpw, a.source, "no")
        verify("генератор после блокировки адреса", check, cpw, "", "ok")
    res["ok"] = "skipped" not in res and all(x["ok"] for x in res["checks"])
    with open(a.out_json, "w", encoding="utf-8") as f:
        json.dump(res, f, ensure_ascii=False, indent=1)
    for x in res["checks"]:
        print(f"{x['name']}: {x['result']} (ожидалось {x['expect']})")
    print(res.get("skipped", f"итог: {'прошёл' if res['ok'] else 'не прошёл'}"))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["run", "idle", "count", "brute"])
    p.add_argument("--accounts", required=True)
    p.add_argument("--host", required=True)
    p.add_argument("--domain", required=True)
    p.add_argument("--prefix", default="user")
    p.add_argument("--users", type=int, required=True)
    p.add_argument("--big", default="big")
    p.add_argument("--ca", default="")
    p.add_argument("--interval", type=float, default=1.0)
    p.add_argument("--out", default="")
    p.add_argument("--out-json", default="")
    p.add_argument("--stop", default="")
    p.add_argument("--events", default="")
    p.add_argument("-n", type=int, default=100)
    p.add_argument("--hold", type=int, default=60)
    p.add_argument("--exclude", action="append", default=[])
    p.add_argument("--source", default="")
    p.add_argument("--target", default="brute")
    p.add_argument("--check", default="")
    p.add_argument("--ip-failures", type=int, default=10)
    p.add_argument("--account-failures", type=int, default=20)
    a = p.parse_args()
    {"run": run, "idle": idle, "count": count, "brute": brute}[a.mode](a)


if __name__ == "__main__":
    sys.exit(main())
