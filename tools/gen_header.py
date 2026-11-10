#!/usr/bin/env python3
"""Генерирует C-заголовок sdk/pcdock-ffi/include/pcdock.h из исходников pcdock-ffi.

Читает `#[no_mangle] pub extern "C" fn`, `pub const PCDOCK_*` и непрозрачные структуры `Pcdock*`.
Запуск:  python3 tools/gen_header.py          — переписать заголовок
         python3 tools/gen_header.py --check  — упасть, если заголовок устарел (для CI)
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "sdk" / "pcdock-ffi" / "src"
OUT = ROOT / "sdk" / "pcdock-ffi" / "include" / "pcdock.h"
FILES = ["lib.rs", "registry.rs", "live.rs"]

SCALARS = {
    "u8": "uint8_t", "u16": "uint16_t", "u32": "uint32_t", "u64": "uint64_t",
    "i32": "int32_t", "i64": "int64_t", "usize": "size_t", "f32": "float", "bool": "bool",
}


def ctype(rust: str) -> str:
    rust = rust.strip()
    m = re.fullmatch(r"\*(mut|const)\s+(.+)", rust)
    if m:
        inner = ctype(m.group(2))
        const = "const " if m.group(1) == "const" else ""
        return f"{const}{inner} *"
    if rust in SCALARS:
        return SCALARS[rust]
    if re.fullmatch(r"Pcdock\w+", rust):
        return rust
    raise SystemExit(f"gen_header: неизвестный тип {rust!r}")


def split_params(text: str):
    depth, cur, out = 0, "", []
    for ch in text:
        if ch in "<(":
            depth += 1
        elif ch in ">)":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return [p.strip() for p in out if p.strip()]


def fmt_param(p: str) -> str:
    name, typ = p.split(":", 1)
    t = ctype(typ)
    return f"{t}{'' if t.endswith('*') else ' '}{name.strip()}"


def doc_before(text: str, pos: int) -> str:
    lines = text[:pos].rstrip().splitlines()
    docs = []
    i = len(lines) - 1
    # пропускаем атрибуты между документом и функцией
    while i >= 0 and lines[i].strip().startswith("#["):
        i -= 1
    while i >= 0 and lines[i].strip().startswith("///"):
        docs.append(lines[i].strip()[3:].strip())
        i -= 1
    return "\n".join(f" * {d}" if d else " *" for d in reversed(docs))


def main() -> int:
    structs, consts, funcs = [], [], []
    fn_re = re.compile(
        r'#\[no_mangle\]\s*(?:#\[[^\]]*\]\s*)*pub extern "C" fn (\w+)\((.*?)\)\s*(?:->\s*([^{]+?))?\s*\{', re.S
    )
    for name in FILES:
        text = (SRC / name).read_text()
        structs += re.findall(r"pub struct (Pcdock\w+)\s*\{", text)
        for m in re.finditer(r"pub const (PCDOCK_\w+): (\w+) = ([^;]+);", text):
            consts.append((m.group(1), m.group(3).strip()))
        for m in fn_re.finditer(text):
            ret = ctype(m.group(3)) if m.group(3) else "void"
            params = [fmt_param(p) for p in split_params(m.group(2))] or ["void"]
            doc = doc_before(text, text.rfind("#[no_mangle]", 0, m.end()))
            funcs.append((m.group(1), ret, params, doc))
    seen, uniq = set(), []
    for c in consts:
        if c[0] not in seen:
            seen.add(c[0])
            uniq.append(c)

    out = [
        "/* Сгенерировано tools/gen_header.py из sdk/pcdock-ffi — не править руками. */",
        "#ifndef PCDOCK_H",
        "#define PCDOCK_H",
        "",
        "#include <stdbool.h>",
        "#include <stddef.h>",
        "#include <stdint.h>",
        "",
        "#ifdef __cplusplus",
        'extern "C" {',
        "#endif",
        "",
    ]
    for s in sorted(set(structs)):
        out.append(f"typedef struct {s} {s};")
    out.append("")
    for n, v in uniq:
        out.append(f"#define {n} {v}")
    out.append("")
    for name, ret, params, doc in funcs:
        if doc:
            out += ["/*", doc, " */"]
        sep = "" if ret.endswith("*") else " "
        out.append(f"{ret}{sep}{name}({', '.join(params)});")
        out.append("")
    out += ["#ifdef __cplusplus", "}", "#endif", "", "#endif /* PCDOCK_H */", ""]
    text = "\n".join(out)

    if "--check" in sys.argv:
        if not OUT.exists() or OUT.read_text() != text:
            print("pcdock.h устарел: запустите python3 tools/gen_header.py", file=sys.stderr)
            return 1
        return 0
    OUT.write_text(text)
    print(f"{OUT.relative_to(ROOT)}: {len(funcs)} функций, {len(uniq)} констант")
    return 0


if __name__ == "__main__":
    sys.exit(main())
