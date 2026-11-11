"""Поставка без интерпретаторов (решение 2026-11-01, SRS 7.2; CI-05).

python interp_check.py <архив.tar[.gz]> ... | -        (- — tar из stdin, например `docker export`)
python interp_check.py --self-test

Нарушение — запись архива:
- интерпретатор по имени (файл, ссылка): оболочки, busybox, python, perl, ruby, node, lua, php, tcl, awk, pwsh;
- обычный файл, начинающийся с `#!` (сценарий), при любых правах;
- исполняемый обычный файл не ELF (сценарий без `#!` запускает оболочка).
Каталоги библиотек и неисполняемые данные без `#!` не нарушение: без интерпретатора их нечем исполнить.
Код возврата 1 и `::error::` на каждое нарушение.
"""

import io
import re
import sys
import tarfile

INTERP = re.compile(
    r"^(sh|bash|dash|ash|zsh|ksh|mksh|csh|tcsh|fish|busybox|toybox|pwsh"
    r"|python[0-9.]*|pypy[0-9.]*|perl[0-9.]*|ruby[0-9.]*|node|nodejs|deno|bun"
    r"|lua[0-9.]*|luajit|php[0-9.]*|tclsh[0-9.]*|wish[0-9.]*|[gmn]?awk)$"
)


def violations(tar: tarfile.TarFile) -> list[str]:
    found = []
    for m in tar:
        name = re.sub(r"^(\./)+|^/+", "", m.name)
        base = name.rsplit("/", 1)[-1]
        if not (m.isfile() or m.issym() or m.islnk()):
            continue
        if INTERP.match(base):
            found.append(f"{name}: интерпретатор")
            continue
        if not m.isfile():
            continue
        f = tar.extractfile(m)
        head = f.read(4) if f else b""
        if head.startswith(b"#!"):
            found.append(f"{name}: сценарий (#!)")
        elif m.mode & 0o111 and head != b"\x7fELF":
            found.append(f"{name}: исполняемый файл не ELF")
    return found


def check(path: str) -> list[str]:
    if path == "-":
        with tarfile.open(fileobj=sys.stdin.buffer, mode="r|*") as t:
            return violations(t)
    with tarfile.open(path, mode="r:*") as t:
        return violations(t)


def _tar(entries: list[tuple[str, bytes, int, str | None]]) -> tarfile.TarFile:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as t:
        for name, data, mode, link in entries:
            ti = tarfile.TarInfo(name)
            ti.mode = mode
            if link is not None:
                ti.type, ti.linkname = tarfile.SYMTYPE, link
                t.addfile(ti)
            else:
                ti.size = len(data)
                t.addfile(ti, io.BytesIO(data))
    buf.seek(0)
    return tarfile.open(fileobj=buf, mode="r|")


def self_test() -> int:
    elf = b"\x7fELF" + b"\0" * 60
    clean = [
        ("usr/local/bin/thepost", elf, 0o755, None),
        ("usr/lib/x86_64-linux-gnu/libc.so.6", elf, 0o755, None),
        ("etc/passwd", b"root:x:0:0::/root:/sbin/nologin\n", 0o644, None),
        ("usr/share/doc/README", b"text\n", 0o644, None),
    ]
    bad = {
        "script.sh (#!, 0644)": ("opt/run.sh", b"#!/bin/sh\necho\n", 0o644, None),
        "script (#!, 0755)": ("usr/local/bin/hook", b"#!/usr/bin/env python3\n", 0o755, None),
        "без #!, 0755": ("usr/local/bin/start", b"echo hi\n", 0o755, None),
        "bin/sh": ("bin/sh", elf, 0o755, None),
        "ссылка busybox": ("bin/ash", b"", 0o777, "busybox"),
        "python3.11": ("usr/bin/python3.11", elf, 0o755, None),
        "perl": ("usr/bin/perl", elf, 0o755, None),
    }
    ok = True
    if v := violations(_tar(clean)):
        print(f"self-test: ложное срабатывание: {v}")
        ok = False
    for label, entry in bad.items():
        if not violations(_tar(clean + [entry])):
            print(f"self-test: не найдено: {label}")
            ok = False
    print("self-test: ok" if ok else "self-test: FAIL")
    return 0 if ok else 1


def main() -> int:
    args = sys.argv[1:]
    if args == ["--self-test"]:
        return self_test()
    if not args:
        print(__doc__)
        return 2
    bad = 0
    for path in args:
        v = check(path)
        for line in v:
            print(f"::error::{path}: {line}")
        print(f"{path}: {'нарушений ' + str(len(v)) if v else 'интерпретаторов и сценариев нет'}")
        bad += len(v)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
