#!/usr/bin/env python3
"""Prove a refactor was a **pure move**: same function bodies, different files.

Task #125 splits `app_core_impl.rs` into concern-scoped `impl AppCore` blocks. Rust allows
multiple impl blocks across files in one crate, so every method can move with zero change to
call sites, types or visibility — which means the refactor can be *proven* behaviour-preserving
rather than merely tested. `cargo test` is the backstop, not the proof: a passing suite would
not notice a method that was silently dropped if nothing covered it.

The check: extract every function in the crate, normalise away file boundaries and ordering,
and compare the multiset of (name, body) pairs before and after. Identical ⇒ nothing was
added, removed, reordered-into-a-different-meaning, or edited. **A non-empty diff is a bug,
not a judgement call.**

    # before touching anything
    python scripts/verify-pure-move.py snapshot crates/pb-app-core/src > /tmp/before.json
    # ...move code...
    python scripts/verify-pure-move.py check crates/pb-app-core/src /tmp/before.json

Deliberately *not* an AST parse: no syntect/syn dependency, and a byte-level comparison is
stricter than a structural one. It does need to know where a function body ends, so the brace
matcher skips string literals, char literals, raw strings and comments — the places a `{` can
appear without opening a block. Without that, `println!("{}")` alone would derail it.

Limits, stated so they are not mistaken for guarantees:
  * It compares FUNCTIONS. Changes to `use` statements, struct definitions, constants, trait
    impl headers or attributes are invisible to it. Those still need review.
  * It cannot tell a method moved between two `impl` blocks for *different types*. #125 moves
    everything within `impl AppCore`, so that is out of scope here, but do not reuse this
    blindly for a refactor that splits a type.
  * Whitespace inside a body is significant (that is intentional — a "pure move" should not
    reflow bodies; run `cargo fmt` before the snapshot so both sides are already formatted).
"""

import hashlib
import json
import sys
from collections import Counter
from pathlib import Path

FN_TOKENS = ('fn ', 'fn\t')


def _strip_scan(src: str, i: int):
    """If `src[i]` starts a string/char/comment, return the index just past it, else None."""
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
        # A char literal, or a lifetime (`'a`). Lifetimes have no closing quote.
        j = i + 1
        if j < len(src) and src[j] == '\\':
            j += 2
        elif j < len(src):
            j += 1
        if j < len(src) and src[j] == "'":
            return j + 1
        return None  # lifetime — not a literal, keep scanning normally
    return None


def _body_end(src: str, open_brace: int) -> int:
    """Index just past the `}` matching the `{` at `open_brace`."""
    depth, i = 0, open_brace
    while i < len(src):
        skip = _strip_scan(src, i)
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


def functions(src: str):
    """Yield (name, body_text) for every `fn` with a body. Nested fns come out too, as part
    of their parent's body AND on their own — harmless, since both sides see the same."""
    i = 0
    while True:
        i = src.find('fn ', i)
        if i == -1:
            return
        # Must be a real token, not the tail of an identifier like `my_fn `.
        if i > 0 and (src[i - 1].isalnum() or src[i - 1] == '_'):
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
        brace = src.find('{', j)
        semi = src.find(';', j)
        if brace == -1 or (semi != -1 and semi < brace):
            i = j  # trait method declaration, no body
            continue
        try:
            end = _body_end(src, brace)
        except ValueError:
            i = j
            continue
        yield name, src[brace:end]
        i = end


def snapshot(root: Path) -> dict:
    entries = Counter()
    files = sorted(p for p in root.rglob('*.rs'))
    for p in files:
        src = p.read_text(encoding='utf-8')
        for name, body in functions(src):
            digest = hashlib.sha256(body.encode('utf-8')).hexdigest()[:16]
            entries[f'{name}:{digest}'] += 1
    return {
        'root': str(root),
        'files': len(files),
        'total': sum(entries.values()),
        'entries': dict(sorted(entries.items())),
    }


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    mode, root = sys.argv[1], Path(sys.argv[2])
    if mode == 'snapshot':
        json.dump(snapshot(root), sys.stdout, indent=1)
        return 0
    if mode != 'check' or len(sys.argv) < 4:
        print(__doc__)
        return 2

    before = json.loads(Path(sys.argv[3]).read_text(encoding='utf-8'))
    after = snapshot(root)
    b, a = Counter(before['entries']), Counter(after['entries'])
    gone, added = b - a, a - b

    print(f"files {before['files']} -> {after['files']}   "
          f"functions {before['total']} -> {after['total']}")
    if not gone and not added:
        print('PURE MOVE VERIFIED: every function body is byte-identical; '
              'only their file locations changed.')
        return 0

    print('\nNOT A PURE MOVE — the following differ:\n')
    for key, n in sorted(gone.items()):
        print(f'  MISSING/CHANGED  {key}  (x{n})')
    for key, n in sorted(added.items()):
        print(f'  NEW/CHANGED      {key}  (x{n})')
    print('\nA name appearing in both lists with different hashes = its body was edited.\n'
          'A name only in MISSING = it was dropped. Only in NEW = it was invented.')
    return 1


if __name__ == '__main__':
    sys.exit(main())
