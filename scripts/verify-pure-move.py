#!/usr/bin/env python3
"""Check that a refactor **conserved every function item** — same text, different files.

Task #125 splits `app_core_impl.rs` into concern-scoped `impl AppCore` blocks. Rust lets an
inherent impl span several modules in one crate, so methods can move with no change to call
sites or types. This tool checks that nothing was dropped, invented or edited on the way.

    python scripts/verify-pure-move.py snapshot crates/pb-app-core/src > before.json
    # ...move code...
    python scripts/verify-pure-move.py check crates/pb-app-core/src before.json
    python scripts/verify-pure-move.py selftest      # the tool's own tests

## ⚠ What this does NOT prove (Codex review, 2026-07-20 — read before trusting it)

It verifies **textual conservation of function items, not behavioural equivalence.** Identical
text can behave differently after a move. Known gaps, all real:

  * **Imports and scope.** An unchanged `foo()` or `.method()` can resolve to a *different*
    function, trait method, const or macro in the destination module. `app_core_impl.rs` has a
    glob `use crate::engine::*`, which makes this a live concern rather than a theoretical one.
  * **Module-sensitive macros.** `file!()`, `line!()`, `column!()`, `module_path!()` and
    relative `include_str!`/`include_bytes!` change meaning with identical text.
  * **Same-name swaps.** Records are keyed by unqualified name, so two same-named functions in
    different modules could exchange bodies and the multiset would not move. (The per-name file
    map in the report is there to make that visible to a human.)
  * **Non-function items.** Structs, consts, statics, type aliases, `mod` declarations, macro
    definitions and trait/impl headers are untracked entirely.
  * **Impl target.** It cannot tell that a method moved between impls for *different types*.
    #125 moves only within `impl AppCore`; do not reuse this blindly for a split that does.
  * **Config and codegen.** Nothing about `#[cfg]` resolution per platform/feature, or about
    inlining and performance.
  * **The parser is hand-written**, not `syn`. It is tested (`selftest`) against the cases that
    have actually bitten, but it is not a Rust front end.

Since 2026-07-20 the hash covers **attributes + signature + body**, not just the body, so
visibility, generics, `async`/`unsafe`/`const`, parameter and return types, and `#[cfg]` /
`#[inline]` / `#[track_caller]` changes DO fail the check. That closes the two largest gaps in
the original version.

So: `cargo test`, clippy, and a real run remain the behavioural evidence. This is a
conservation check that catches the class of mistake a test suite is worst at — a silently
dropped item nothing covers.
"""

import hashlib
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path


def _skip_literal_or_comment(src: str, i: int):
    """If `src[i]` opens a string/char/comment, return the index just past it, else None."""
    two = src[i:i + 2]
    if two == '//':
        j = src.find('\n', i)
        return len(src) if j == -1 else j
    if two == '/*':
        depth, j = 1, i + 2
        while j < len(src) and depth:
            if src[j:j + 2] == '/*':
                depth, j = depth + 1, j + 2
            elif src[j:j + 2] == '*/':
                depth, j = depth - 1, j + 2
            else:
                j += 1
        return j
    if src[i] == 'r' and i + 1 < len(src) and src[i + 1] in '#"':
        j = i + 1
        hashes = 0
        while j < len(src) and src[j] == '#':
            hashes, j = hashes + 1, j + 1
        if j < len(src) and src[j] == '"':
            close = '"' + '#' * hashes
            k = src.find(close, j + 1)
            return len(src) if k == -1 else k + len(close)
        return None
    if src[i] == '"':
        j = i + 1
        while j < len(src):
            if src[j] == '\\':
                j += 2
                continue
            if src[j] == '"':
                return j + 1
            j += 1
        return len(src)
    if src[i] == "'":
        # A char literal, or a lifetime (`'a`) — lifetimes have no closing quote.
        j = i + 1
        if j < len(src) and src[j] == '\\':
            j += 2
        elif j < len(src):
            j += 1
        if j < len(src) and src[j] == "'":
            return j + 1
        return None
    return None


def _sig_terminator(src: str, start: int):
    """From just past a fn name, find the `{` opening its body or the `;` ending a bodyless
    declaration — whichever comes first **at bracket depth 0**.

    Depth tracking is the fix for the bug Codex found: `fn f() -> [u8; 3] {` has a `;` inside
    the array type, and a naive `find(';')` read that as a trait declaration and skipped the
    function entirely. It was invisible to the check and could have been dropped undetected.
    """
    depth, i = 0, start
    while i < len(src):
        skip = _skip_literal_or_comment(src, i)
        if skip is not None and skip > i:
            i = skip
            continue
        ch = src[i]
        if ch in '([':
            depth += 1
        elif ch in ')]':
            depth -= 1
        elif depth == 0 and ch == '{':
            return '{', i
        elif depth == 0 and ch == ';':
            return ';', i
        i += 1
    return None, -1


def _body_end(src: str, open_brace: int) -> int:
    depth, i = 0, open_brace
    while i < len(src):
        skip = _skip_literal_or_comment(src, i)
        if skip is not None and skip > i:
            i = skip
            continue
        if src[i] == '{':
            depth += 1
        elif src[i] == '}':
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    raise ValueError(f'unbalanced braces from offset {open_brace}')


def _item_start(src: str, fn_kw: int) -> int:
    """Walk back from the `fn` keyword over the signature's modifiers, then over any
    contiguous attribute / doc-comment lines, so the hash covers them."""
    line_start = src.rfind('\n', 0, fn_kw) + 1
    start = line_start
    while True:
        prev = src.rfind('\n', 0, start - 1) + 1 if start > 0 else 0
        if prev == start:
            break
        line = src[prev:start].strip()
        if line.startswith(('#[', '#![', '///', '//!', '//')):
            start = prev
            continue
        break
    return start


def functions(src: str):
    """Yield (name, item_text) for every `fn` with a body. `item_text` spans attributes,
    doc comments, the full signature and the body."""
    i = 0
    while True:
        i = src.find('fn ', i)
        if i == -1:
            return
        if i > 0 and (src[i - 1].isalnum() or src[i - 1] == '_'):
            i += 3
            continue
        # Ignore `fn` inside a comment or string.
        line_start = src.rfind('\n', 0, i) + 1
        stripped = src[line_start:i].lstrip()
        if stripped.startswith(('///', '//!', '//')):
            i += 3
            continue
        name_start = i + 3
        j = name_start
        while j < len(src) and (src[j].isalnum() or src[j] == '_'):
            j += 1
        name = src[name_start:j]
        if not name:
            i += 3
            continue
        kind, pos = _sig_terminator(src, j)
        if kind != '{':
            i = j if pos == -1 else pos + 1  # bodyless declaration
            continue
        try:
            end = _body_end(src, pos)
        except ValueError:
            i = j
            continue
        yield name, src[_item_start(src, i):end]
        i = end


def snapshot(root: Path) -> dict:
    entries = Counter()
    where = defaultdict(set)
    files = sorted(p for p in root.rglob('*.rs'))
    for p in files:
        src = p.read_text(encoding='utf-8')
        for name, item in functions(src):
            digest = hashlib.sha256(item.encode('utf-8')).hexdigest()[:16]
            entries[f'{name}:{digest}'] += 1
            where[name].add(p.name)
    return {
        'root': str(root),
        'files': len(files),
        'total': sum(entries.values()),
        'entries': dict(sorted(entries.items())),
        'locations': {k: sorted(v) for k, v in sorted(where.items())},
    }


def check(root: Path, before_path: Path) -> int:
    before = json.loads(before_path.read_text(encoding='utf-8'))
    after = snapshot(root)
    b, a = Counter(before['entries']), Counter(after['entries'])
    gone, added = b - a, a - b

    print(f"files {before['files']} -> {after['files']}   "
          f"function items {before['total']} -> {after['total']}")

    if not gone and not added:
        moved = [n for n, locs in after['locations'].items()
                 if before.get('locations', {}).get(n, locs) != locs]
        print('FUNCTION-ITEM CONSERVATION VERIFIED: every attribute+signature+body is '
              'byte-identical; only file locations changed.')
        if moved:
            print(f'  relocated: {len(moved)} name(s) — {", ".join(sorted(moved)[:12])}'
                  f'{" …" if len(moved) > 12 else ""}')
        print('  NOTE: this is textual conservation, not behavioural equivalence. See the '
              'module docstring for what it cannot see (scope/imports, module-sensitive '
              'macros, non-function items, same-name swaps).')
        return 0

    print('\nCONSERVATION FAILED — the following differ:\n')
    for key, n in sorted(gone.items()):
        print(f'  MISSING/CHANGED  {key}  (x{n})')
    for key, n in sorted(added.items()):
        print(f'  NEW/CHANGED      {key}  (x{n})')
    print('\nSame name in both lists = its text was edited (attributes, signature or body).\n'
          'Only in MISSING = dropped. Only in NEW = invented.')
    return 1


# ─────────────────────────────── self-tests ───────────────────────────────
CASES = [
    ('array return type (the bug Codex found)',
     'impl A {\n    pub fn letterbox(&self) -> [u8; 3] {\n        [0, 0, 0]\n    }\n}\n',
     ['letterbox']),
    ('bodyless trait declaration is skipped',
     'trait T {\n    fn decl(&self) -> u32;\n    fn with_body(&self) -> u32 { 1 }\n}\n',
     ['with_body']),
    ('brace inside a string literal',
     'fn f() {\n    println!("{}", 1);\n}\n',
     ['f']),
    ('`fn ` inside a comment is not an item',
     '// fn ghost() {}\n/// fn also_ghost() {}\nfn real() {}\n',
     ['real']),
    ('raw string containing braces and quotes',
     'fn g() {\n    let _ = r#"a { " b"#;\n}\n',
     ['g']),
    ('lifetime is not a char literal',
     "fn h<'a>(x: &'a str) -> &'a str {\n    x\n}\n",
     ['h']),
    # A nested fn is NOT yielded separately: it lives inside its parent's item text, so a
    # change to it changes the parent's hash. Covered once, not twice.
    ('nested fn is covered via its parent, not listed separately',
     'fn outer() {\n    fn inner() {}\n    inner();\n}\n',
     ['outer']),
    ('semicolon in a generic array param',
     'fn k(v: [u8; 4]) -> Option<[u8; 2]> {\n    None\n}\n',
     ['k']),
]


def selftest() -> int:
    ok = True
    for label, src, expect in CASES:
        got = [n for n, _ in functions(src)]
        good = sorted(got) == sorted(expect)
        ok &= good
        print(f"  {'PASS' if good else 'FAIL'}  {label}")
        if not good:
            print(f'        expected {sorted(expect)}, got {sorted(got)}')

    # Sensitivity: an attribute-only change must alter the hash (it did not, pre-2026-07-20).
    a = list(functions('fn f() { 1 }\n'))[0][1]
    b = list(functions('#[inline]\nfn f() { 1 }\n'))[0][1]
    sens = a != b
    ok &= sens
    print(f"  {'PASS' if sens else 'FAIL'}  an added #[attribute] changes the item text")

    # Sensitivity: a visibility change must alter the hash.
    c = list(functions('pub fn f() { 1 }\n'))[0][1]
    vis = a != c
    ok &= vis
    print(f"  {'PASS' if vis else 'FAIL'}  a visibility change alters the item text")
    print('selftest:', 'all passed' if ok else 'FAILURES')
    return 0 if ok else 1


def main() -> int:
    if len(sys.argv) >= 2 and sys.argv[1] == 'selftest':
        return selftest()
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    mode, root = sys.argv[1], Path(sys.argv[2])
    if mode == 'snapshot':
        json.dump(snapshot(root), sys.stdout, indent=1)
        return 0
    if mode == 'check' and len(sys.argv) >= 4:
        return check(root, Path(sys.argv[3]))
    print(__doc__)
    return 2


if __name__ == '__main__':
    sys.exit(main())
