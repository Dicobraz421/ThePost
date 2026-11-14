"""Контраст ролей фирменного стиля (решение 119, DES-01).

python design_contrast.py [design/tokens.toml]
python design_contrast.py --self-test

Проверяет design/tokens.toml:
- в каждом цветном режиме ([mode.*], кроме mono) заданы все роли из [roles], значения разбираются;
- пары [[contrast]] в каждом цветном режиме — не ниже min (WCAG 2.x: 4,5 у текста, 3 у рамок и значков);
- красный (тон в пределах 20° от 0°, насыщенный) — только у ролей [rules].red_roles; в c16 цвета 1 и 9 — тоже;
- [mode.mono] — только атрибуты bold, dim, reverse, underline;
- [cli]: у каждой роли brand не ниже [cli].min на каждом фоне [cli].backgrounds, ansi — параметры SGR,
  attrs согласованы с ansi (1 — bold, 2 — dim), красный — только у [cli.rules].red_roles.
Код возврата 1 и `::error::` на каждое нарушение. Зависимостей нет (tomllib, Python 3.11+).
"""

import colorsys
import copy
import re
import sys
import tomllib

HEX = re.compile(r"^#[0-9A-F]{6}$")
SGR = re.compile(r"^[0-9]+(;[0-9]+)*$")
MONO_ATTRS = {"bold", "dim", "reverse", "underline"}
RED_SLOTS16 = {1, 9}


def hex_rgb(h: str) -> tuple[int, int, int]:
    return int(h[1:3], 16), int(h[3:5], 16), int(h[5:7], 16)


def luminance(h: str) -> float:
    def lin(c: int) -> float:
        s = c / 255
        return s / 12.92 if s <= 0.04045 else ((s + 0.055) / 1.055) ** 2.4

    r, g, b = hex_rgb(h)
    return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)


def contrast(a: str, b: str) -> float:
    hi, lo = sorted((luminance(a), luminance(b)), reverse=True)
    return (hi + 0.05) / (lo + 0.05)


def is_red(h: str) -> bool:
    r, g, b = (c / 255 for c in hex_rgb(h))
    hue, light, sat = colorsys.rgb_to_hls(r, g, b)
    deg = hue * 360
    return sat >= 0.45 and 0.15 <= light <= 0.85 and (deg <= 20 or deg >= 340)


def xterm256(i: int) -> str:
    """Цвет xterm-256 с номером 16–255 (куб 6×6×6 и серая шкала)."""
    if 16 <= i <= 231:
        levels = [0, 95, 135, 175, 215, 255]
        i -= 16
        rgb = (levels[i // 36], levels[i // 6 % 6], levels[i % 6])
    else:
        g = 8 + 10 * (i - 232)
        rgb = (g, g, g)
    return "#%02X%02X%02X" % rgb


def resolve(kind: str, value, palette16: list[str]) -> str:
    """Значение роли в режиме → #RRGGBB; ValueError, если не разбирается."""
    if kind == "c256":
        if not isinstance(value, int) or isinstance(value, bool) or not 16 <= value <= 255:
            raise ValueError(f"{value!r}: нужен номер 16–255")
        return xterm256(value)
    if kind == "c16":
        if not isinstance(value, int) or isinstance(value, bool) or not 0 <= value <= 15:
            raise ValueError(f"{value!r}: нужен номер 0–15")
        return palette16[value]
    if not isinstance(value, str) or not HEX.match(value):
        raise ValueError(f"{value!r}: нужен #RRGGBB")
    return value


def mode_kind(name: str) -> str:
    return name if name in ("c256", "c16", "mono") else "truecolor"


def check(tok: dict) -> list[str]:
    """Нарушения в разобранном tokens.toml; пустой список — всё в порядке."""
    errs: list[str] = []
    roles = list(tok.get("roles", {}))
    if not roles:
        return ["[roles] пуст"]
    for name, h in tok.get("brand", {}).items():
        if not isinstance(h, str) or not HEX.match(h):
            errs.append(f"[brand].{name}: {h!r} — нужен #RRGGBB")
    for key in ("ok", "warn", "err"):
        if not tok.get("signs", {}).get(key):
            errs.append(f"[signs].{key} не задан")
    for key in ("heading", "text"):
        font = tok.get("fonts", {}).get(key, {})
        if not font.get("family") or not font.get("weights"):
            errs.append(f"[fonts.{key}]: нужны family и weights")
    red_roles = set(tok.get("rules", {}).get("red_roles", []))
    palette16 = tok.get("palette16", {}).get("colors", [])
    if len(palette16) != 16 or not all(isinstance(h, str) and HEX.match(h) for h in palette16):
        errs.append("[palette16].colors: нужно 16 значений #RRGGBB")
        palette16 = ["#000000"] * 16
    modes = tok.get("mode", {})
    for need in ("dark", "light", "c256", "c16", "mono"):
        if need not in modes:
            errs.append(f"[mode.{need}] не задан")
    pairs = tok.get("contrast", [])
    if not pairs:
        errs.append("[[contrast]] пуст")

    for mname, mode in modes.items():
        kind = mode_kind(mname)
        missing = [r for r in roles if r not in mode]
        extra = [r for r in mode if r not in roles]
        if missing:
            errs.append(f"[mode.{mname}]: нет ролей {', '.join(missing)}")
        if extra:
            errs.append(f"[mode.{mname}]: роли вне [roles]: {', '.join(extra)}")
        if kind == "mono":
            for r, attrs in mode.items():
                if not isinstance(attrs, list) or not set(attrs) <= MONO_ATTRS:
                    errs.append(f"[mode.mono].{r}: {attrs!r} — только {sorted(MONO_ATTRS)}")
            continue
        colors: dict[str, str] = {}
        for r, v in mode.items():
            try:
                colors[r] = resolve(kind, v, palette16)
            except ValueError as e:
                errs.append(f"[mode.{mname}].{r}: {e}")
                continue
            if r not in red_roles:
                if is_red(colors[r]):
                    errs.append(f"[mode.{mname}].{r}: красный {colors[r]} — только у {sorted(red_roles)}")
                if kind == "c16" and v in RED_SLOTS16:
                    errs.append(f"[mode.{mname}].{r}: красный цвет {v} — только у {sorted(red_roles)}")
        for p in pairs:
            fg, bgs, need = p.get("fg"), p.get("bg", []), p.get("min")
            for bg in bgs:
                if fg not in colors or bg not in colors:
                    continue
                ratio = contrast(colors[fg], colors[bg])
                if ratio < need:
                    errs.append(
                        f"[mode.{mname}]: {fg} {colors[fg]} на {bg} {colors[bg]} — {ratio:.2f}:1, нужно {need}:1"
                    )

    errs.extend(check_cli(tok.get("cli")))
    return errs


def check_cli(cli: dict | None) -> list[str]:
    if cli is None:
        return ["[cli] не задан"]
    errs: list[str] = []
    bgs, need = cli.get("backgrounds", []), cli.get("min")
    if not bgs or not all(isinstance(h, str) and HEX.match(h) for h in bgs) or not need:
        return ["[cli]: нужны backgrounds (#RRGGBB) и min"]
    red_roles = set(cli.get("rules", {}).get("red_roles", []))
    roles = cli.get("roles", {})
    if not roles:
        errs.append("[cli.roles] пуст")
    for r, spec in roles.items():
        ansi, brand, attrs = spec.get("ansi"), spec.get("brand"), spec.get("attrs")
        if not isinstance(ansi, str) or not SGR.match(ansi):
            errs.append(f"[cli.roles].{r}: ansi {ansi!r} — нужны параметры SGR, например \"1;94\"")
            ansi = ""
        if not isinstance(attrs, list) or not set(attrs) <= MONO_ATTRS:
            errs.append(f"[cli.roles].{r}: attrs {attrs!r} — только {sorted(MONO_ATTRS)}")
            attrs = []
        codes = set(ansi.split(";")) if ansi else set()
        for code, attr in (("1", "bold"), ("2", "dim")):
            if (code in codes) != (attr in attrs):
                errs.append(f"[cli.roles].{r}: ansi {ansi!r} и attrs {attrs!r} расходятся в {attr}")
        if not isinstance(brand, str) or not HEX.match(brand):
            errs.append(f"[cli.roles].{r}: brand {brand!r} — нужен #RRGGBB")
            continue
        if r not in red_roles and is_red(brand):
            errs.append(f"[cli.roles].{r}: красный {brand} — только у {sorted(red_roles)}")
        for bg in bgs:
            ratio = contrast(brand, bg)
            if ratio < need:
                errs.append(f"[cli.roles].{r}: {brand} на {bg} — {ratio:.2f}:1, нужно {need}:1")
    return errs


def report(path: str) -> int:
    with open(path, "rb") as f:
        tok = tomllib.load(f)
    errs = check(tok)
    for e in errs:
        print(f"::error file={path}::{e}")
    if errs:
        return 1
    n_modes = sum(1 for m in tok["mode"] if mode_kind(m) != "mono")
    print(f"{path}: контраст в порядке — {n_modes} цветных режима, {len(tok['contrast'])} правил, "
          f"CLI {len(tok['cli']['roles'])} ролей")
    return 0


def self_test(path: str) -> int:
    """Проверка падает на испорченных токенах и проходит на настоящих."""
    with open(path, "rb") as f:
        good = tomllib.load(f)
    fails = 0
    if errs := check(good):
        print(f"::error::{path} не проходит: {errs[0]}")
        fails += 1

    def spoiled(desc: str, edit, expect: str) -> None:
        nonlocal fails
        tok = copy.deepcopy(good)
        edit(tok)
        errs = check(tok)
        if not any(expect in e for e in errs):
            print(f"::error::испорченный файл ({desc}) прошёл проверку; нарушения: {errs}")
            fails += 1
        else:
            print(f"ok: {desc} — найдено")

    spoiled("текст ниже 4,5:1", lambda t: t["mode"]["light"].update(text="#9AA6B2"), "text #9AA6B2 на")
    spoiled("рамка ниже 3:1", lambda t: t["mode"]["dark"].update(frameA="#26313B"), "frameA #26313B на")
    spoiled("номер 256 ниже порога", lambda t: t["mode"]["c256"].update(muted=236), "[mode.c256]: muted")
    spoiled("красный не у ошибки", lambda t: t["mode"]["dark"].update(head="#FF3030"), "head: красный")
    spoiled("красный цвет 16 не у ошибки", lambda t: t["mode"]["c16"].update(warn=9), "warn: красный цвет 9")
    spoiled("нет роли", lambda t: t["mode"]["dark"].pop("ok"), "нет ролей ok")
    spoiled("атрибут монохрома", lambda t: t["mode"]["mono"].update(sel=["blink"]), "[mode.mono].sel")
    spoiled("CLI на светлом фоне", lambda t: t["cli"]["roles"]["ok"].update(brand="#1FA5A3"),
            "#1FA5A3 на #FAFAFA")
    spoiled("CLI на тёмном фоне", lambda t: t["cli"]["roles"]["muted"].update(brand="#3A4652"),
            "#3A4652 на #0C0C0C")
    spoiled("CLI ansi и attrs", lambda t: t["cli"]["roles"]["head"].update(attrs=[]), "расходятся в bold")
    return 1 if fails else 0


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--self-test":
        return self_test(args[1] if len(args) > 1 else "design/tokens.toml")
    return report(args[0] if args else "design/tokens.toml")


if __name__ == "__main__":
    sys.exit(main())
