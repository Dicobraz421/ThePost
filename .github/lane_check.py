"""Проверка полос (скилл pets-dev): PR правит файлы одной полосы.

python lane_check.py lanes.toml changed.txt <head_ref>
Файлы из [common] разрешены в любом PR. Ветка x/* — межполосный PR класса B (только отчёт).
Файл вне всех полос считается полосой "unmapped" — добавьте его в lanes.toml.
"""

import fnmatch
import sys
import tomllib


def lane_of(path: str, lanes: dict[str, list[str]]) -> str:
    for name, globs in lanes.items():
        if any(fnmatch.fnmatch(path, g) for g in globs):
            return name
    return "unmapped"


def main() -> int:
    cfg_path, changed_path, head_ref = sys.argv[1], sys.argv[2], sys.argv[3]
    with open(cfg_path, "rb") as f:
        cfg = tomllib.load(f)
    common = cfg.get("common", {}).get("paths", [])
    lanes = {k: v["paths"] for k, v in cfg.get("lane", {}).items()}
    with open(changed_path, encoding="utf-8") as f:
        files = [x.strip() for x in f if x.strip()]

    touched: dict[str, list[str]] = {}
    for p in files:
        if any(fnmatch.fnmatch(p, g) for g in common):
            continue
        touched.setdefault(lane_of(p, lanes), []).append(p)

    for lane, paths in sorted(touched.items()):
        print(f"{lane}: {len(paths)} files")
    if len(touched) <= 1:
        return 0
    if head_ref.startswith("x/"):
        print("::notice::межполосный PR (x/*) — нужен approve владельца")
        return 0
    print(f"::error::PR правит {len(touched)} полосы: {', '.join(sorted(touched))}. "
          "Разбейте на волны или назовите ветку x/<суть> (класс B).")
    return 1


if __name__ == "__main__":
    sys.exit(main())
