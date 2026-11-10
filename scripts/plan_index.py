#!/usr/bin/env python3
"""Сводка плана: docs/tasks/README.md (карточки со статусами) и блок «Обзор этапов» в docs/PLAN.md.

Источник статуса — отметки ✅ у волн в docs/PLAN.md: карточка готова, если её волна отмечена.
Запуск из корня репозитория: `python3 scripts/plan_index.py`; проверка без записи — `--check`.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLAN = ROOT / "docs/PLAN.md"
TASKS = ROOT / "docs/tasks"
INDEX = TASKS / "README.md"
BEGIN, END = "<!-- overview:begin -->", "<!-- overview:end -->"

PHASES = [  # этап, содержание (SRS APP-G), критерий завершения
    ("Ф0", "Основа", "Платформа, хранилище метаданных и BLOB, CI, стенд SMB",
     "Узел SMB запускается из одного бинарного файла; метаданные и BLOB с тестами; CI ≤ 7 мин; стенд SMB поднимается"),
    ("Ф1", "MVP SMB", "SMTP с очередью и Milter, IMAP, JMAP Mail, поиск, LDAP, Kerberos, OAUTHBEARER, корзина, Admin API, CLI",
     "Почта SMB целиком на стенде: приём и отправка, IMAP и JMAP, поиск, вход; проверка клиентами Thunderbird и Postman; нагрузка Д.6 для SMB"),
    ("Ф2", "Совместная работа", "CalDAV, CardDAV, JMAP Calendars и Contacts, scheduling, free/busy, Sieve, веб-консоль администратора",
     "Календари, контакты, правила и автоответы проверены клиентами; консоль администратора покрывает Admin API"),
    ("Ф3", "Enterprise", "S3, кластерные роли, межузловое взаимодействие, edge, шарды, Legal Hold, экспорт и импорт",
     "Схемы Enterprise-ВМ и Enterprise-K8s (приложение З) на стенде; переключение СУБД ≤ 30 с; нагрузка Д.6 для Enterprise"),
    ("Ф4", "Выпуск", "Стабилизация, аудит безопасности, фаззинг парсеров, документация администратора",
     "Аудит безопасности без открытых критичных находок; фаззинг парсеров; документация; выпуск 1.0"),
]


def cards():
    out = {}
    for f in sorted(TASKS.glob("*.md")):
        if f.name == "README.md":
            continue
        text = f.read_text(encoding="utf-8")
        title = text.splitlines()[0].split(":", 1)[-1].strip() if text else f.stem
        field = lambda k: (re.search(rf"^{k}:\s*([^#\n]*)", text, re.M) or [None, ""])[1].strip()
        out[f.stem] = {"title": title, "lane": field("lane"), "class": field("class"),
                       "deps": field("deps").strip("[]").replace(" ", "") or "—"}
    return out


def plan_waves():
    """Секции PLAN → волны: (этап, заголовок секции, имя волны, готова, карточки, ждёт)."""
    phase, section, rows, done = None, None, [], set()
    for line in PLAN.read_text(encoding="utf-8").splitlines():
        h = re.match(r"^(##+) (.+)", line)
        if h:
            section = h.group(2).strip()
            p = re.match(r"(Ф\d)", section)
            if h.group(1) == "##":
                phase = p.group(1) if p else None
            continue
        m = re.match(r"^\| (✅ )?([a-z]+-\d+)\b[^|]*\| ([^|]+)\| ([^|]+)\| ([^|]+)\|", line)
        if m and phase:
            name = m.group(2)
            if m.group(1):
                done.add(name)
            rows.append((phase, section, name, m.group(3).strip(), m.group(5).strip()))
    return rows, done


def status(name, waits, done):
    if name in done:
        return "done"
    deps = re.findall(r"[a-z]+-\d+", waits)
    return "ready" if all(d in done for d in deps) else "todo"


def build():
    cs, (rows, done) = cards(), plan_waves()
    lines = ["# Задачи — карточки", "",
             "Генерируется `scripts/plan_index.py` из `docs/PLAN.md` (отметки ✅ у волн) и карточек; вручную не править.",
             "Статусы: done — волна смёржена; ready — зависимости закрыты, можно запускать; todo — ждёт зависимостей.", ""]
    per_phase = {}
    cur = None
    for phase, section, name, cardlist, waits in rows:
        st = status(name, waits, done)
        ids = re.findall(r"[A-Z]+-\d+", cardlist)
        pp = per_phase.setdefault(phase, [0, 0, 0, 0])
        pp[0] += 1
        pp[1] += st == "done"
        pp[2] += len(ids)
        pp[3] += len(ids) if st == "done" else 0
        if (phase, section) != cur:
            cur = (phase, section)
            lines += ["", f"## {section}" if section.startswith(phase) else f"## {phase} · {section}", "",
                      "| Задача | Суть | Полоса | Класс | Волна | Статус | Зависит |", "|---|---|---|---|---|---|---|"]
        for cid in ids:
            c = cs.get(cid, {"title": "—", "lane": "—", "class": "—", "deps": "—"})
            lines.append(f"| [{cid}]({cid}.md) | {c['title']} | {c['lane']} | {c['class']} | {name} | {st} | {c['deps']} |")
    planned = {cid for _, _, _, cl, _ in rows for cid in re.findall(r"[A-Z]+-\d+", cl)}
    extra = sorted(set(cs) - planned)
    if extra:
        lines += ["", "## Вне волн", "", "| Задача | Суть | Полоса | Класс | Зависит |", "|---|---|---|---|---|"]
        lines += [f"| [{c}]({c}.md) | {cs[c]['title']} | {cs[c]['lane']} | {cs[c]['class']} | {cs[c]['deps']} |" for c in extra]
    ov = [BEGIN, "## Обзор этапов", "",
          "Статус волн и карточек — `docs/tasks/README.md` (генерируется `scripts/plan_index.py`).", "",
          "| Этап | Содержание | Волны | Карточки | Статус | Критерий завершения |", "|---|---|---|---|---|---|"]
    for ph, name, content, crit in PHASES:
        w, wd, k, kd = per_phase.get(ph, [0, 0, 0, 0])
        if w == 0:
            st = "карточки — после Ф1"
        elif wd == w:
            st = "✅ готово"
        elif wd == 0:
            st = "не начат"
        else:
            st = f"в работе, {round(100 * kd / k)} % карточек"
        ov.append(f"| {ph}. {name} | {content} | {wd}/{w} | {kd}/{k} | {st} | {crit} |")
    ov += ["", "Открытые вопросы владельцу — issue с меткой «решение»; дополнительные модули — раздел в конце.", END]
    return "\n".join(lines) + "\n", "\n".join(ov)


def main():
    index, overview = build()
    plan = PLAN.read_text(encoding="utf-8")
    if BEGIN in plan:
        plan = re.sub(re.escape(BEGIN) + ".*?" + re.escape(END), overview, plan, flags=re.S)
    else:
        plan = plan.replace("\n## Процесс", "\n" + overview + "\n\n## Процесс", 1)
    if "--check" in sys.argv:
        stale = INDEX.read_text(encoding="utf-8") != index if INDEX.exists() else True
        stale |= PLAN.read_text(encoding="utf-8") != plan
        sys.exit("сводка устарела: python3 scripts/plan_index.py" if stale else 0)
    INDEX.write_text(index, encoding="utf-8")
    PLAN.write_text(plan, encoding="utf-8")


if __name__ == "__main__":
    main()
