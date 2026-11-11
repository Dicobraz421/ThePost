#!/usr/bin/env python3
"""Наполнение ящиков стенда SMB до прогона (APP-D Д.3, DEP-12): письма корпуса на порт 25, parallel сеансов SMTP,
не быстрее rate писем/с; ящикам нагрузки — по messages писем, ящику big — big-messages малых писем (критерий
«IMAP SELECT папки 10 тыс. писем»). Принятое (250) — в state (JSON, ящик → писем): повтор досылает только
недостающее. Итог — принято за запуск и всего, видно в ящиках (IMAP STATUS, выборка verify ящиков; 0 — все).

Запуск: fill.py --accounts <login:пароль> --corpus <каталог> --host <узел> --users <N> --prefix user
        --messages 1500 --big big@stand.test --big-messages 10000 --state fill-state.json [--ca ca.pem]
"""

import argparse
import glob
import imaplib
import json
import os
import random
import smtplib
import ssl
import sys
import threading
import time
from email.message import EmailMessage

SAVE_EVERY = 1.0


def load_accounts(path):
    out = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            login, _, pw = line.rstrip("\n").partition(":")
            if login:
                out[login] = pw
    return out


def small_message(n, words, rng):
    msg = EmailMessage()
    msg["From"] = f"bulk{n}@external.test"
    msg["To"] = "big@stand.test"
    msg["Subject"] = " ".join(rng.choice(words) for _ in range(4))
    msg["Message-ID"] = f"<stand-big-{n}-{rng.getrandbits(32):08x}@external.test>"
    msg.set_content(" ".join(rng.choice(words) for _ in range(rng.randint(40, 120))) + "\n")
    return msg.as_bytes()


class Limiter:
    """Не быстрее rate событий в секунду на все потоки."""

    def __init__(self, rate):
        self.step = 1.0 / rate if rate > 0 else 0.0
        self.next = time.monotonic()
        self.lock = threading.Lock()

    def wait(self):
        with self.lock:
            now = time.monotonic()
            at = max(self.next, now)
            self.next = at + self.step
        if at > now:
            time.sleep(at - now)


class State:
    def __init__(self, path):
        self.path = path
        self.done = {}
        if os.path.exists(path):
            with open(path, encoding="utf-8") as f:
                self.done = json.load(f)
        self.lock = threading.Lock()
        self.saved = time.monotonic()

    def add(self, login):
        with self.lock:
            self.done[login] = self.done.get(login, 0) + 1
            if time.monotonic() - self.saved > SAVE_EVERY:
                self._save()

    def _save(self):
        tmp = self.path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(self.done, f, sort_keys=True)
        os.replace(tmp, self.path)
        self.saved = time.monotonic()

    def save(self):
        with self.lock:
            self._save()


def deliver(args, jobs, state, limiter, corpus, words, stats):
    rng = random.Random()
    smtp = None
    while True:
        with stats["lock"]:
            if not jobs:
                break
            login, n = jobs.pop()
        data = small_message(n, words, rng) if login == args.big else rng.choice(corpus)
        for attempt in range(5):
            limiter.wait()
            try:
                if smtp is None:
                    smtp = smtplib.SMTP(args.host, args.port, timeout=60)
                    smtp.ehlo("fill.external.test")
                smtp.sendmail(f"fill@{args.sender_domain}", [login], data)
                state.add(login)
                with stats["lock"]:
                    stats["accepted"] += 1
                break
            except (smtplib.SMTPException, OSError) as e:
                with stats["lock"]:
                    stats["errors"] += 1
                    stats["last_error"] = f"{type(e).__name__}: {e}"[:200]
                try:
                    if smtp is not None:
                        smtp.close()
                except OSError:
                    pass
                smtp = None
                time.sleep(min(30, 2**attempt))
        else:
            with stats["lock"]:
                stats["failed"] += 1
    if smtp is not None:
        try:
            smtp.quit()
        except (smtplib.SMTPException, OSError):
            pass


def visible(host, login, pw, ctx):
    m = imaplib.IMAP4_SSL(host, 993, ssl_context=ctx, timeout=60)
    try:
        m.login(login, pw)
        typ, data = m.status("INBOX", "(MESSAGES)")
        if typ != "OK":
            raise RuntimeError(f"STATUS {login}: {typ}")
        return int(data[0].decode().rsplit("MESSAGES", 1)[1].strip(" )"))
    finally:
        try:
            m.logout()
        except (imaplib.IMAP4.error, OSError):
            pass


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--accounts", required=True)
    p.add_argument("--corpus", required=True)
    p.add_argument("--host", required=True)
    p.add_argument("--port", type=int, default=25)
    p.add_argument("--ca", default="")
    p.add_argument("--users", type=int, required=True)
    p.add_argument("--prefix", default="user")
    p.add_argument("--domain", required=True)
    p.add_argument("--messages", type=int, default=1500)
    p.add_argument("--big", default="")
    p.add_argument("--big-messages", type=int, default=10000)
    p.add_argument("--parallel", type=int, default=25)
    p.add_argument("--rate", type=float, default=50)
    p.add_argument("--state", required=True)
    p.add_argument("--verify", type=int, default=20)
    p.add_argument("--sender-domain", default="external.test")
    args = p.parse_args()

    accounts = load_accounts(args.accounts)
    targets = {f"{args.prefix}{n}@{args.domain}": args.messages for n in range(1, args.users + 1)}
    if args.big and args.big_messages > 0:
        targets[args.big] = args.big_messages
    missing = [a for a in targets if a not in accounts]
    if missing:
        sys.exit(f"нет пароля учётных записей ({len(missing)}, например {missing[0]}): сначала --tags provision")

    corpus = []
    for path in sorted(glob.glob(os.path.join(args.corpus, "msg-*.eml"))):
        with open(path, "rb") as f:
            corpus.append(f.read())
    with open(os.path.join(args.corpus, "words.txt"), encoding="utf-8") as f:
        words = [w for w in f.read().split() if w]
    state = State(args.state)
    jobs = []
    for login, want in targets.items():
        have = state.done.get(login, 0)
        jobs.extend((login, n) for n in range(have + 1, want + 1))
    random.shuffle(jobs)
    planned = len(jobs)
    stats = {"lock": threading.Lock(), "accepted": 0, "errors": 0, "failed": 0, "last_error": ""}
    started = time.monotonic()
    if jobs:
        limiter = Limiter(args.rate)
        threads = [
            threading.Thread(target=deliver, args=(args, jobs, state, limiter, corpus, words, stats))
            for _ in range(min(args.parallel, planned))
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    state.save()
    took = time.monotonic() - started
    total = sum(state.done.get(a, 0) for a in targets)
    print(
        f"fill: planned={planned} delivered={stats['accepted']} failed={stats['failed']} retries={stats['errors']} "
        f"in {took:.0f}s; accepted total={total} of {sum(targets.values())} (mailboxes={len(targets)})"
    )
    if stats["last_error"]:
        print(f"fill: last error: {stats['last_error']}")

    # Сверка: видно в ящиках (доставка из очереди — с задержкой, ожидание до 300 с).
    ctx = ssl.create_default_context(cafile=args.ca or None)
    sample = list(targets)
    if 0 < args.verify < len(sample):
        sample = random.sample([a for a in sample if a != args.big], args.verify - 1) + (
            [args.big] if args.big in targets else []
        )
    deadline = time.monotonic() + 300
    while True:
        seen = {a: visible(args.host, a, accounts[a], ctx) for a in sample}
        short = {a: state.done.get(a, 0) - seen[a] for a in sample if seen[a] < state.done.get(a, 0)}
        if not short or time.monotonic() > deadline:
            break
        time.sleep(10)
    expect = sum(state.done.get(a, 0) for a in sample)
    print(f"fill: visible={sum(seen.values())} of accepted={expect} (IMAP STATUS INBOX, mailboxes={len(sample)})")
    if args.big in seen:
        print(f"fill: big mailbox {args.big}: {seen[args.big]} messages")
    if short:
        print(f"fill: short mailboxes={len(short)}, например {next(iter(short))}: -{next(iter(short.values()))}")
    sys.exit(1 if stats["failed"] or short else 0)


if __name__ == "__main__":
    main()
