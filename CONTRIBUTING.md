# Contributing to Blaze Viewer

Patches are welcome. Before you spend a weekend on one, please read this — it
will tell you honestly whether your change is likely to land.

## The bar

Blaze Viewer has exactly one prime directive:

> **Will this make it faster, or have basically zero performance impact?**
> If it's neither, it doesn't ship.

That is not a slogan. It is the acceptance criterion, and it is stricter than
most projects'. A patch that adds a feature at the cost of a millisecond in the
navigation path will be declined — not because the feature is bad, but because
speed *is* the product. If you want a viewer that does everything, there are
excellent ones. This one does one thing.

**Corollary: we don't guess about speed, we measure it.** Any change touching
the hot path (decode, upload, prefetch, present) needs numbers from the
benchmark corpus, not reasoning about why it ought to be faster. "This should
reduce allocations" is not evidence. A benchmark diff is.

See [`CLAUDE.md`](./CLAUDE.md) for the performance model and the architecture it
forces. It's worth reading before you write code — most rejected ideas are
rejected for reasons documented there.

## What lands easily

- **Bug fixes.** Especially with a failing test that goes green.
- **Format support** and decode-path correctness (wrong colors, bad EXIF
  orientation, a container we mis-sniff).
- **Platform fixes** — Windows and Linux get less of my personal wall-clock than
  macOS, and it shows.
- **Docs**, including telling us this document is wrong.

## What's a harder sell

- Anything that adds UI. The entire visible surface is deliberately tiny.
- Refactors without a measured payoff.
- New dependencies. Every one is a build-risk and a license question.
- Anything in the hot path without benchmarks.

Open an issue *before* writing code for these. A ten-minute conversation is
cheaper than a rejected weekend, and I would rather say "no" to a paragraph than
to your Saturday.

## Before you open a PR

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

All three must pass. Tests follow TDD: the pure logic in `pb-core` is unit-tested
and the GPU path is covered by headless golden-image tests.

> **Note on crate names:** the crates are prefixed `pb-` (`pb-app`, `pb-decode`,
> `pb-render`, …) — a holdover from the project's original name. They're internal
> identifiers, they aren't user-visible, and renaming them is churn with no
> payoff. They're staying. Don't "fix" them.

## Licensing — the CLA

Blaze Viewer is **source-available and commercially licensed** (see
[LICENSE.md](./LICENSE.md)). Binaries are sold. That means contributed code ends
up in a product we charge for, which means we need clear rights to it — not to
be greedy, but because "we shipped a paying customer a binary containing code we
have unclear rights to" is a genuinely bad place to be, for us *and* for you.

**So: substantial contributions require a CLA.** It's a one-click sign on your
first PR — a bot will comment with the link. You keep the copyright to your work;
you grant us the rights to ship it.

**Trivial changes don't.** Typos, one-liners, obvious null checks, doc fixes —
just send them. Copyright doesn't meaningfully attach to `if x.is_none()`, and
making you sign a legal document to fix a comment would be theatre.

If you'd rather not sign, that's completely fine and not a judgement: **open an
issue describing the problem instead of a PR containing the fix.** A well-written
bug report is genuinely valuable, and it's often faster than review.

## Code of conduct

Be decent. Assume the other person is smart and busy. Disagree about code as much
as you like; that's the fun part.

## Maintainer's honest disclaimer

This is a solo project run by one person with a job, two small kids, and a
finite number of evenings. Review may be slow. Silence is not disdain, it's
bandwidth. If a PR goes quiet for two weeks, a nudge is welcome and not rude.
