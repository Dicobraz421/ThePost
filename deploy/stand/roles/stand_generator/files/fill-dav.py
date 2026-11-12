#!/usr/bin/env python3
"""Наполнение календарей и адресных книг стенда SMB (DEP-21, по образцу наполнения ящиков DEP-12): каждой учётной
записи нагрузки — events событий в календарь по умолчанию (CalDAV PUT, доля recurring — еженедельные повторы) и
contacts карточек в адресную книгу по умолчанию (CardDAV PUT), parallel потоков, не быстрее rate запросов/с.
Имена ресурсов постоянные (stand-<n>.ics, stand-<n>.vcf) и PUT с If-None-Match: * — повтор не дублирует (412 —
уже есть); выполненное — в state (JSON, как у fill.py: «<логин>#events» → число): полная коллекция пропускается,
неполная проходится заново с первого ресурса.
Итог — записано за запуск и всего, видно в коллекциях (PROPFIND Depth: 1, выборка verify учётных записей).

Запуск: fill-dav.py --accounts <login:пароль> --host <узел> --users <N> --prefix user --domain stand.test
        --events 300 --contacts 100 --state fill-dav-state.json [--ca ca.pem]
"""

import argparse
import base64
import http.client
import random
import re
import ssl
import sys
import threading
import time
import urllib.parse
from datetime import datetime, timedelta, timezone

from fill import Limiter, State, load_accounts

RESPONSE = re.compile(rb"<(?:[A-Za-z0-9]+:)?response[\s>]")
PROPFIND = (b'<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop>'
            b"</d:propfind>")
FIRST = ["Анна", "Борис", "Вера", "Глеб", "Дарья", "Егор", "Жанна", "Иван", "Ксения", "Лев", "Мария", "Олег"]
LAST = ["Иванов", "Петров", "Сидоров", "Кузнецов", "Смирнов", "Попов", "Волков", "Зайцев", "Морозов", "Орлов"]


def calendar(login):
    return f"/dav/calendars/{urllib.parse.quote(login, safe='@')}/calendar/"


def addressbook(login):
    return f"/dav/addressbooks/{urllib.parse.quote(login, safe='@')}/contacts/"


def event(login, n, rng, recurring):
    start = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0) + timedelta(
        days=rng.randint(-180, 180), hours=rng.randint(-4, 4))
    end = start + timedelta(minutes=rng.choice([30, 60, 90]))
    fmt = "%Y%m%dT%H%M%SZ"
    words = " ".join(rng.choice(["план", "обзор", "встреча", "отчёт", "созвон", "проект", "бюджет"]) for _ in range(3))
    rule = "RRULE:FREQ=WEEKLY;COUNT=20\r\n" if recurring else ""
    return (
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//The Post//stand//RU\r\nBEGIN:VEVENT\r\n"
        f"UID:stand-{n}-{login}\r\nDTSTAMP:{datetime.now(timezone.utc).strftime(fmt)}\r\n"
        f"DTSTART:{start.strftime(fmt)}\r\nDTEND:{end.strftime(fmt)}\r\n{rule}"
        f"SUMMARY:{words.capitalize()}\r\nDESCRIPTION:Событие стенда {n}\r\n"
        "END:VEVENT\r\nEND:VCALENDAR\r\n"
    ).encode()


def card(login, n, rng):
    first, last = rng.choice(FIRST), rng.choice(LAST)
    return (
        "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//The Post//stand//RU\r\n"
        f"UID:stand-{n}-{login}\r\nFN:{first} {last}\r\nN:{last};{first};;;\r\n"
        f"EMAIL;TYPE=WORK:contact{n}@external.test\r\nTEL;TYPE=CELL:+7900{rng.randint(1000000, 9999999)}\r\n"
        "ORG:Стенд\r\nEND:VCARD\r\n"
    ).encode()


class Dav:
    """Соединение HTTPS учётной записи; переподключение — на каждой ошибке."""

    def __init__(self, args, ctx):
        self.args, self.ctx, self.conn = args, ctx, None

    def request(self, login, pw, method, path, body, headers):
        if self.conn is None:
            self.conn = http.client.HTTPSConnection(self.args.host, 443, context=self.ctx, timeout=60)
        h = {"Authorization": "Basic " + base64.b64encode(f"{login}:{pw}".encode()).decode()}
        h.update(headers)
        try:
            self.conn.request(method, path, body=body, headers=h)
            r = self.conn.getresponse()
            data = r.read()
        except (OSError, http.client.HTTPException):
            self.close()
            raise
        if r.getheader("Connection", "").lower() == "close":
            self.close()
        return r.status, data

    def close(self):
        if self.conn is not None:
            self.conn.close()
            self.conn = None


def put(args, jobs, accounts, state, limiter, stats, ctx):
    rng = random.Random()
    dav = Dav(args, ctx)
    while True:
        with stats["lock"]:
            if not jobs:
                break
            login, kind, n = jobs.pop()
        if kind == "events":
            path = f"{calendar(login)}stand-{n}.ics"
            body, ctype = event(login, n, rng, rng.random() < args.recurring), "text/calendar; charset=utf-8"
        else:
            path = f"{addressbook(login)}stand-{n}.vcf"
            body, ctype = card(login, n, rng), "text/vcard; charset=utf-8"
        for attempt in range(5):
            limiter.wait()
            try:
                code, data = dav.request(login, accounts[login], "PUT", path, body,
                                         {"Content-Type": ctype, "If-None-Match": "*"})
                if code in (201, 204, 412):
                    state.add(f"{login}#{kind}")
                    with stats["lock"]:
                        stats["written" if code != 412 else "existing"] += 1
                    break
                raise RuntimeError(f"PUT {path}: {code} {data[:120]!r}")
            except (OSError, http.client.HTTPException, RuntimeError) as e:
                with stats["lock"]:
                    stats["errors"] += 1
                    stats["last_error"] = f"{type(e).__name__}: {e}"[:200]
                time.sleep(min(30, 2**attempt))
        else:
            with stats["lock"]:
                stats["failed"] += 1
    dav.close()


def visible(args, ctx, login, pw, path):
    """Ресурсов в коллекции (PROPFIND Depth: 1 без самой коллекции)."""
    dav = Dav(args, ctx)
    try:
        code, data = dav.request(login, pw, "PROPFIND", path, PROPFIND,
                                 {"Depth": "1", "Content-Type": "application/xml; charset=utf-8"})
    finally:
        dav.close()
    if code != 207:
        raise RuntimeError(f"PROPFIND {path}: {code}")
    return len(RESPONSE.findall(data)) - 1


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--accounts", required=True)
    p.add_argument("--host", required=True)
    p.add_argument("--ca", default="")
    p.add_argument("--users", type=int, required=True)
    p.add_argument("--prefix", default="user")
    p.add_argument("--domain", required=True)
    p.add_argument("--events", type=int, default=300)
    p.add_argument("--contacts", type=int, default=100)
    p.add_argument("--recurring", type=float, default=0.1)
    p.add_argument("--parallel", type=int, default=25)
    p.add_argument("--rate", type=float, default=50)
    p.add_argument("--state", required=True)
    p.add_argument("--verify", type=int, default=20)
    args = p.parse_args()

    accounts = load_accounts(args.accounts)
    logins = [f"{args.prefix}{n}@{args.domain}" for n in range(1, args.users + 1)]
    missing = [a for a in logins if a not in accounts]
    if missing:
        sys.exit(f"нет пароля учётных записей ({len(missing)}, например {missing[0]}): сначала --tags provision")
    ctx = ssl.create_default_context(cafile=args.ca or None)
    want = {"events": args.events, "contacts": args.contacts}
    state = State(args.state)
    jobs = []
    for login in logins:
        for kind, total in want.items():
            if state.done.get(f"{login}#{kind}", 0) < total:
                state.done[f"{login}#{kind}"] = 0
                jobs.extend((login, kind, n) for n in range(1, total + 1))
    random.shuffle(jobs)
    planned = len(jobs)
    stats = {"lock": threading.Lock(), "written": 0, "existing": 0, "errors": 0, "failed": 0, "last_error": ""}
    started = time.monotonic()
    if jobs:
        # Дома и коллекции по умолчанию создаются при первом обращении к дому (PROPFIND).
        for login in logins:
            for kind in ("calendars", "addressbooks"):
                path = f"/dav/{kind}/{urllib.parse.quote(login, safe='@')}/"
                visible(args, ctx, login, accounts[login], path)
        limiter = Limiter(args.rate)
        threads = [
            threading.Thread(target=put, args=(args, jobs, accounts, state, limiter, stats, ctx))
            for _ in range(min(args.parallel, planned))
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    state.save()
    took = time.monotonic() - started
    total = {k: sum(state.done.get(f"{a}#{k}", 0) for a in logins) for k in want}
    print(
        f"fill-dav: planned={planned} written={stats['written']} existing={stats['existing']} failed={stats['failed']} "
        f"retries={stats['errors']} in {took:.0f}s; events total={total['events']} of {args.events * len(logins)}, "
        f"contacts total={total['contacts']} of {args.contacts * len(logins)} (accounts={len(logins)})"
    )
    if stats["last_error"]:
        print(f"fill-dav: last error: {stats['last_error']}")

    sample = logins if args.verify <= 0 or args.verify >= len(logins) else random.sample(logins, args.verify)
    short = 0
    seen = {"events": 0, "contacts": 0}
    for login in sample:
        for kind, path in (("events", calendar(login)), ("contacts", addressbook(login))):
            n = visible(args, ctx, login, accounts[login], path)
            seen[kind] += n
            short += n < state.done.get(f"{login}#{kind}", 0)
    expect = {k: sum(state.done.get(f"{a}#{k}", 0) for a in sample) for k in want}
    print(f"fill-dav: visible events={seen['events']} of {expect['events']}, contacts={seen['contacts']} of "
          f"{expect['contacts']} (PROPFIND, accounts={len(sample)}, short collections={short})")
    sys.exit(1 if stats["failed"] or short else 0)


if __name__ == "__main__":
    main()
