#!/usr/bin/env python3
"""Сверка доски Project v2 «ThePost — план-график» с PR репозитория и таблицей волн docs/PLAN.md.

Запуск: python3 -I scripts/board_sync.py [--plan docs/PLAN.md] [--apply]
Без --apply только печатает, что поменял бы. Нужен `gh` с токеном, у которого есть запись в проекты
организации (GH_TOKEN); встроенный GITHUB_TOKEN к доскам организации доступа не имеет.

Правила:
- PR с веткой `w/<волна>` или `x/<волна>` из этого репозитория: открытый — «В работе», влитый — «Сделано»;
  закрытые без мержа на доску не добавляются и не меняются. PR из форков не учитываются.
- Волна — часть ветки после `w/`/`x/`; полоса — латинский префикс волны до первой цифры или дефиса;
  класс — `x/` → B, `w/` → A. Этап нового PR берётся из черновика той же волны, иначе — Ф2.
- Полосы, которой нет среди вариантов поля «Полоса», поле остаётся пустым (варианты не добавляем:
  это пересоздаёт поле и рискует уже заполненными значениями) — пишется строка «НЕТ ПОЛОСЫ».
- Черновик волны («<волна>: …») переводится в «Сделано», когда влит PR этой волны.
- Невыполненная строка таблицы PLAN.md без PR и без карточки на доске — новый черновик в «План».
Все значения передаются переменными GraphQL, в текст запроса ничего не подставляется.
"""
import argparse
import json
import re
import subprocess
import sys

REPO = "Dicobraz421/ThePost"
PROJECT = "PVT_kwDOFK5ZS84BrKj4"  # Project #3 организации Dicobraz421
STATUS, STAGE, LANE, CLASS, WAVE, START, END = "Status", "Этап", "Полоса", "Класс", "Волна", "Начало", "Окончание"
ST_PLAN, ST_WORK, ST_DONE = "План", "В работе", "Сделано"
DEFAULT_STAGE = "Ф2. Совместная работа"

FIELD_VALUE = {
    "single": "{singleSelectOptionId:$v}",
    "text": "{text:$v}",
    "date": "{date:$v}",
}


def gql(query, **variables):
    body = json.dumps({"query": query, "variables": variables})
    r = subprocess.run(["gh", "api", "graphql", "--input", "-"], input=body,
                       capture_output=True, text=True, encoding="utf-8")
    if r.returncode:
        raise SystemExit(f"gh api graphql: {r.stderr or r.stdout}")
    d = json.loads(r.stdout)
    if d.get("errors"):
        raise SystemExit(f"GraphQL: {d['errors']}")
    return d["data"]


def fields():
    """Поля проекта по имени: {имя: (id, {вариант: id})}."""
    q = """query($p:ID!){node(id:$p){... on ProjectV2{fields(first:50){nodes{
      ... on ProjectV2FieldCommon{id name} ... on ProjectV2SingleSelectField{options{id name}}}}}}}"""
    out = {}
    for f in gql(q, p=PROJECT)["node"]["fields"]["nodes"]:
        if f:
            out[f["name"]] = (f["id"], {o["name"]: o["id"] for o in f.get("options") or []})
    missing = [n for n in (STATUS, STAGE, LANE, CLASS, WAVE, START, END) if n not in out]
    if missing:
        raise SystemExit(f"на доске нет полей: {', '.join(missing)}")
    return out


def items():
    q = """query($p:ID!,$c:String){node(id:$p){... on ProjectV2{items(first:100,after:$c){
      pageInfo{hasNextPage endCursor}
      nodes{id content{__typename ... on PullRequest{number} ... on Issue{number} ... on DraftIssue{title}}
        fieldValues(first:20){nodes{
          ... on ProjectV2ItemFieldTextValue{text field{... on ProjectV2FieldCommon{id}}}
          ... on ProjectV2ItemFieldSingleSelectValue{optionId field{... on ProjectV2FieldCommon{id}}}}}}}}}}"""
    out, cur = [], None
    while True:
        d = gql(q, p=PROJECT, c=cur)["node"]["items"]
        for n in d["nodes"]:
            fv = {f["field"]["id"]: f.get("text") or f.get("optionId")
                  for f in n["fieldValues"]["nodes"] if f and f.get("field")}
            out.append({"id": n["id"], "c": n["content"] or {}, "f": fv})
        if not d["pageInfo"]["hasNextPage"]:
            return out
        cur = d["pageInfo"]["endCursor"]


def prs():
    r = subprocess.run(["gh", "pr", "list", "--repo", REPO, "--state", "all", "--limit", "10000", "--json",
                        "number,id,headRefName,isCrossRepository,state,createdAt,mergedAt"],
                       capture_output=True, text=True, encoding="utf-8")
    if r.returncode:
        raise SystemExit(f"gh pr list: {r.stderr}")
    return [p for p in json.loads(r.stdout) if not p["isCrossRepository"]]


def wave_of(branch):
    m = re.match(r"^[wx]/(.+)$", branch)
    return m.group(1) if m else None


def lane_name(wave):
    m = re.match(r"^([a-z]+)", wave or "")
    return m.group(1) if m else None


PLAN_ROW = re.compile(r"^\|\s*(✅\s*)?([a-z][a-z0-9-]*)(?:\s*\(ветка `([wx]/[^`]+)`\))?\s*\|"
                      r"\s*([^|]+?)\s*\|\s*([AB])\s*\|\s*([^|]*?)\s*\|")


def plan_rows(path):
    rows = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            m = PLAN_ROW.match(line)
            if m:
                rows.append({"done": bool(m.group(1)), "wave": m.group(2), "cards": m.group(4),
                             "cls": m.group(5), "deps": m.group(6)})
    return rows


class Board:
    def __init__(self, apply):
        self.apply = apply
        self.f = fields()
        self.no_lane = set()

    def fid(self, name):
        return self.f[name][0]

    def opt(self, name, option):
        return self.f[name][1].get(option)

    def set(self, item, name, value, kind):
        if not self.apply or value is None:
            return
        gql("mutation($p:ID!,$i:ID!,$f:ID!,$v:%s){updateProjectV2ItemFieldValue(input:{projectId:$p,"
            "itemId:$i,fieldId:$f,value:%s}){projectV2Item{id}}}" % ("String!" if kind != "date" else "Date!",
                                                                    FIELD_VALUE[kind]),
            p=PROJECT, i=item, f=self.fid(name), v=value)

    def set_lane(self, item, wave):
        lane = lane_name(wave)
        option = self.opt(LANE, lane) if lane else None
        if option is None:
            if lane not in self.no_lane:
                self.no_lane.add(lane)
                print(f"НЕТ ПОЛОСЫ «{lane}» среди вариантов поля «{LANE}» — поле оставлено пустым (волна {wave})")
            return
        self.set(item, LANE, option, "single")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--plan", default="docs/PLAN.md")
    ap.add_argument("--apply", action="store_true", help="записать изменения на доску")
    a = ap.parse_args()

    b = Board(a.apply)
    its, allpr, rows = items(), prs(), plan_rows(a.plan)
    st = {s: b.opt(STATUS, s) for s in (ST_PLAN, ST_WORK, ST_DONE)}
    if None in st.values():
        raise SystemExit(f"у поля «{STATUS}» нет вариантов: {[s for s, v in st.items() if v is None]}")
    on_board_pr = {i["c"]["number"]: i for i in its if i["c"].get("__typename") == "PullRequest"}
    drafts = {i["c"]["title"].split(":")[0].strip(): i for i in its if i["c"].get("__typename") == "DraftIssue"}
    known = set(drafts) | {i["f"].get(b.fid(WAVE)) for i in its if i["f"].get(b.fid(WAVE))}
    merged = set()
    stats = {"pr_add": 0, "pr_status": 0, "draft_done": 0, "draft_add": 0}

    for p in allpr:
        w = wave_of(p["headRefName"])
        if not w:
            continue
        if p["state"] == "MERGED":
            merged.add(w)
        want = {"MERGED": ST_DONE, "OPEN": ST_WORK}.get(p["state"])
        if want:
            known.add(w)
        it = on_board_pr.get(p["number"])
        if it is None:
            if not want:
                continue
            stats["pr_add"] += 1
            print(f"ADD PR #{p['number']} {w} → {want}")
            iid = None
            if a.apply:
                iid = gql("mutation($p:ID!,$c:ID!){addProjectV2ItemById(input:{projectId:$p,contentId:$c})"
                          "{item{id}}}", p=PROJECT, c=p["id"])["addProjectV2ItemById"]["item"]["id"]
                b.set(iid, STATUS, st[want], "single")
                b.set(iid, WAVE, w, "text")
                b.set(iid, CLASS, b.opt(CLASS, "B" if p["headRefName"].startswith("x/") else "A"), "single")
                b.set(iid, STAGE, (drafts.get(w) or {}).get("f", {}).get(b.fid(STAGE))
                      or b.opt(STAGE, DEFAULT_STAGE), "single")
                b.set(iid, START, p["createdAt"][:10], "date")
                if p["mergedAt"]:
                    b.set(iid, END, p["mergedAt"][:10], "date")
            b.set_lane(iid, w)
        elif want and it["f"].get(b.fid(STATUS)) != st[want]:
            stats["pr_status"] += 1
            print(f"STATUS PR #{p['number']} {w} → {want}")
            b.set(it["id"], STATUS, st[want], "single")
            if p["mergedAt"]:
                b.set(it["id"], END, p["mergedAt"][:10], "date")

    for w, it in drafts.items():
        if w in merged and it["f"].get(b.fid(STATUS)) != st[ST_DONE]:
            stats["draft_done"] += 1
            print(f"DONE DRAFT {w}")
            b.set(it["id"], STATUS, st[ST_DONE], "single")

    for r in rows:
        if r["done"] or r["wave"] in known:
            continue
        known.add(r["wave"])
        stats["draft_add"] += 1
        title = f'{r["wave"]}: {r["cards"]}'
        print(f"ADD DRAFT {title}")
        iid = None
        if a.apply:
            body = f'Класс {r["cls"]}; ждёт: {r["deps"] or "—"}. Карточки: {r["cards"]}.'
            iid = gql("mutation($p:ID!,$t:String!,$b:String!){addProjectV2DraftIssue(input:{projectId:$p,"
                      "title:$t,body:$b}){projectItem{id}}}", p=PROJECT, t=title, b=body
                      )["addProjectV2DraftIssue"]["projectItem"]["id"]
            b.set(iid, STATUS, st[ST_PLAN], "single")
            b.set(iid, WAVE, r["wave"], "text")
            b.set(iid, CLASS, b.opt(CLASS, r["cls"]), "single")
            b.set(iid, STAGE, b.opt(STAGE, DEFAULT_STAGE), "single")
        b.set_lane(iid, r["wave"])

    print(("применено: " if a.apply else "пробный прогон (без --apply): ") + json.dumps(stats, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
