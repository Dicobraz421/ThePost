#!/usr/bin/env python3
"""Корпус писем стенда по модели APP-D Д.3: средний размер письма, доля писем с вложением и размер вложения,
доля внутренних дублей MIME-частей (части берутся из общего набора — проверка дедупликации).

Запуск: make-corpus.py <каталог> <писем> <размер KB> <доля вложений> <вложение KB> <доля дублей>.
Пишет msg-<n>.eml (CRLF, для smtp-source -F и swaks --data) и imaptest.mbox (источник APPEND imaptest),
words.txt — словарь текста (запросы поиска k6). Детерминирован (seed), повторный запуск даёт те же файлы.
"""

import os
import random
import sys
from email.message import EmailMessage
from email.policy import SMTP
from email.utils import format_datetime
from datetime import datetime, timedelta, timezone

WORDS = (
    "договор счёт поставка отчёт встреча проект бюджет квартал согласование заявка график отпуск склад "
    "логистика сервер резервирование миграция почта календарь приглашение презентация клиент партнёр "
    "contract invoice delivery report meeting project budget quarter approval request schedule warehouse "
    "server backup migration mailbox calendar invitation presentation customer partner energy trading "
    "pipeline forecast gas power market settlement deal desk risk credit portfolio"
).split()


def text(rng: random.Random, size: int) -> str:
    out, n = [], 0
    while n < size:
        line = " ".join(rng.choice(WORDS) for _ in range(rng.randint(8, 14)))
        out.append(line)
        n += len(line.encode()) + 2
    return "\n".join(out) + "\n"


def main() -> None:
    out, count, size_kb, att_share, att_kb, dup_share = sys.argv[1:7]
    # Текст в base64 (UTF-8) растёт на 4/3: заданный размер — у письма, не у текста.
    count, size, att_size = int(count), int(size_kb) * 1024 * 3 // 4, int(att_kb) * 1024
    att_share, dup_share = float(att_share), float(dup_share)
    rng = random.Random(20261110)
    # Общие части: тексты и вложения, повторяющиеся в разных письмах (дубли MIME-частей).
    shared_texts = [text(rng, size) for _ in range(5)]
    shared_atts = [rng.randbytes(att_size) for _ in range(5)]
    base = datetime(2026, 1, 1, tzinfo=timezone.utc)
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "imaptest.mbox"), "wb") as mbox:
        for n in range(1, count + 1):
            dup = rng.random() < dup_share
            msg = EmailMessage(policy=SMTP)
            msg["From"] = f"sender{n}@external.test"
            msg["To"] = "stand@stand.test"
            msg["Subject"] = " ".join(rng.choice(WORDS) for _ in range(4))
            msg["Date"] = format_datetime(base + timedelta(minutes=n))
            msg["Message-ID"] = f"<stand-{n}@external.test>"
            msg.set_content(rng.choice(shared_texts) if dup else text(rng, size))
            if rng.random() < att_share:
                data = rng.choice(shared_atts) if dup else rng.randbytes(att_size)
                msg.add_attachment(data, maintype="application", subtype="octet-stream", filename=f"att-{n}.bin")
            raw = msg.as_bytes()
            with open(os.path.join(out, f"msg-{n:04d}.eml"), "wb") as f:
                f.write(raw)
            mbox.write(b"From stand@external.test Thu Jan  1 00:00:00 2026\n")
            mbox.write(raw.replace(b"\r\n", b"\n").replace(b"\nFrom ", b"\n>From ") + b"\n")
    with open(os.path.join(out, "words.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(WORDS) + "\n")


if __name__ == "__main__":
    main()
