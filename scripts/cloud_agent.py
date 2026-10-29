"""Облачный агент волны The Post (процесс pets-dev): Managed Agent в песочнице Anthropic (Linux).

python cloud_agent.py run <repo> <полоса> <NN> <ID> [<ID> ...] [--budget 10]
python cloud_agent.py wait <session>        # ждать конца хода, напечатать отчёт агента
python cloud_agent.py say <session> <текст> # следующее сообщение в ту же сессию

Нужен CLI `ant` (`ant auth login`). Агент и окружение — PETS_AGENT_ID, PETS_AGENT_ENV.
Токен GitHub для клона и push — PETS_AGENT_GH_TOKEN, иначе `gh auth token`; в песочницу
он не попадает (git-прокси Anthropic). Агент пушит ветку w/<полоса>-<NN>, PR открывает
оркестратор: `gh pr create --head w/<полоса>-<NN>` → `gh pr merge --auto --rebase`.
"""

import argparse
import json
import os
import pathlib
import subprocess
import sys
import time

AGENT = "agent_01F6D4gpkVGwAMFdbECXWrnM"
ENV = "env_01QprbqY855mny5KgiwpRvPt"
ORG = "Dicobraz421"
PROMPT = pathlib.Path(__file__).resolve().parent.parent / "docs" / "process" / "agent-prompt-cloud.md"


def env(name: str, default: str) -> str:
    return os.environ.get(name) or default


def ant(*args: str, body: dict | None = None) -> str:
    """Вызов `ant`; тело запроса — JSON на stdin, чтобы токен не светился в argv."""
    r = subprocess.run(
        ["ant", *args],
        input=json.dumps(body) if body is not None else None,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    if r.returncode != 0:
        sys.exit(f"ant {args[0]}: {r.stderr.strip()}")
    return r.stdout.strip()


def gh_token() -> str:
    t = env("PETS_AGENT_GH_TOKEN", "")
    if t:
        return t
    r = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True)
    if r.returncode != 0 or not r.stdout.strip():
        sys.exit("нет токена GitHub: задайте PETS_AGENT_GH_TOKEN или выполните gh auth login")
    return r.stdout.strip()


def message(text: str) -> dict:
    return {"type": "user.message", "content": [{"type": "text", "text": text}]}


def cmd_run(a: argparse.Namespace) -> None:
    cards = ", ".join(f"docs/tasks/{c}.md" for c in a.cards)
    text = (
        PROMPT.read_text(encoding="utf-8")
        .replace("<repo>", a.repo)
        .replace("<полоса>", a.lane)
        .replace("<NN>", a.wave)
        .replace("<карточки>", cards)
    )
    body = {
        "agent": env("PETS_AGENT_ID", AGENT),
        "environment_id": env("PETS_AGENT_ENV", ENV),
        "title": f"{a.repo} w/{a.lane}-{a.wave}: {' '.join(a.cards)}",
        "resources": [
            {
                "type": "github_repository",
                "url": f"https://github.com/{ORG}/{a.repo}",
                "mount_path": f"/workspace/{a.repo}",
                "authorization_token": gh_token(),
            }
        ],
        "budget": {
            "type": "limit",
            "max_list_cost": {"amount": str(int(a.budget * 100)), "currency": "USD"},
        },
        "initial_events": [message(text)],
    }
    sid = ant("beta:sessions", "create", "--transform", "id", "-r", body=body)
    print(sid)


def events(sid: str) -> list[dict]:
    out = ant("beta:sessions:events", "list", "--session-id", sid, "--format", "jsonl")
    return [json.loads(x) for x in out.splitlines() if x.strip()]


def cmd_wait(a: argparse.Namespace) -> None:
    deadline = time.monotonic() + a.timeout * 60
    while True:
        status = ant("beta:sessions", "retrieve", "--session-id", a.session, "--transform", "status", "-r")
        if status in ("idle", "terminated"):
            break
        if time.monotonic() > deadline:
            sys.exit(f"{a.session}: всё ещё {status} через {a.timeout} мин")
        time.sleep(30)
    evs = events(a.session)
    idle = [e for e in evs if e.get("type") == "session.status_idle"]
    reason = (idle[-1].get("stop_reason") or {}).get("type") if idle else None
    msgs = [e for e in evs if e.get("type") == "agent.message"]
    if msgs:
        print("".join(b.get("text", "") for b in msgs[-1].get("content") or []))
    cost = ant("beta:sessions", "retrieve", "--session-id", a.session, "--transform", "usage.list_cost.amount", "-r")
    print(f"\n[{status}: {reason}, ${int(cost or 0) / 100:.2f}]")
    if reason not in ("end_turn", None):
        sys.exit(2)


def cmd_say(a: argparse.Namespace) -> None:
    ant("beta:sessions:events", "send", "--session-id", a.session, body={"events": [message(a.text)]})


def main() -> None:
    sys.stdout.reconfigure(encoding="utf-8")
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(required=True)
    r = sub.add_parser("run")
    r.add_argument("repo")
    r.add_argument("lane")
    r.add_argument("wave")
    r.add_argument("cards", nargs="+")
    r.add_argument("--budget", type=float, default=10, help="потолок трат сессии, USD")
    r.set_defaults(fn=cmd_run)
    w = sub.add_parser("wait")
    w.add_argument("session")
    w.add_argument("--timeout", type=int, default=90, help="минут")
    w.set_defaults(fn=cmd_wait)
    s = sub.add_parser("say")
    s.add_argument("session")
    s.add_argument("text")
    s.set_defaults(fn=cmd_say)
    a = p.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
