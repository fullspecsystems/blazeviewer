# Where does this code go?

**The rule: "put it on `AppCore`" is the *last* answer, not the first.** Reach for it only
when every option below has been ruled out — and when you do, put it in a concern file under
`app_core_impl/`, never in `app_core_impl.rs` itself.

This exists because the default answer used to be the only answer, and it produced a 22,000-line
file. That file is not a moral failing — it *worked*, and it got the product here. But it became
a liability: LLM tooling chokes on it (two Codex review runs produced nothing at all, exhausting
their budget just reading it), a patch landed in the wrong function because two plausible
neighbours sat 40 lines apart, "what else touches this?" needs a repo-wide grep, and every
concurrent branch conflicts in the same place.

The instructive part is **how** it grew. Task #126 created three new modules
(`background.rs`, `dir_scan.rs`, `archive_open.rs`) — and *still* added ~1,170 lines to
`app_core_impl.rs`, because the **logic** had a home and the **`impl AppCore` methods didn't**.
Nobody was careless. "Where does an `AppCore` method go?" had exactly one answer.

So this document's job is to make sure that question always has a better one.

---

## The decision procedure

Ordered. **First match wins** — stop at the first "yes".

### 1. Is it pure logic — no I/O, no GPU, no platform, no clock?

→ **`pb-core`**, if it is about navigation, playlist, shuffle, prefetch windows, ring residency
or thumbs. That crate is 100% cfg-free and property-tested and **the purity bar is absolute**:
no `std::fs`, no `cfg`, no `unsafe`. If your function would breach that, it does not go there —
find it a home below rather than lowering the bar.

→ Otherwise a **pure module in the owning crate** (`pb-decode`, `pb-source`, `pb-render`…).
A function of its arguments should be a free function in a module, not a method on a god object.

**Test:** could you unit-test it without constructing an `AppCore`? Then it does not want to be
an `AppCore` method.

### 2. Does it belong to a subsystem that already exists?

`pb-app-core` has ~49 modules. The home very likely already exists — check before inventing.

| you are working on | it lives in |
|---|---|
| directory walking, deck building | `scan.rs` |
| archive opening, password flow | `archive.rs`, `archive_open.rs` |
| background worker identity / supersession | `background.rs`, `dir_scan.rs` |
| video playback, sessions, tracks | `video.rs`, `video_session.rs`, `video_native.rs`, `tracks.rs` |
| subtitles | `subtitle.rs`, `subtitle_engine.rs`, `cues.rs` |
| posters | `poster_select.rs` |
| thumbnails | `thumbs.rs` |
| decode scheduling | `decode_pool.rs` |
| folder tree / navigation UI state | `folder_tree.rs`, `fs_tree.rs`, `follow.rs` |
| panels, overlays, toasts | `panels.rs`, `overlay.rs` |
| settings, keymap, launch flags | `settings.rs`, `keymap.rs`, `launch.rs`, `config.rs` |
| file edits (delete, rotation, sidecars) | `delete.rs`, `save_rotation.rs`, `sidecar.rs`, `undo.rs` |
| AI description, OCR | `describe.rs`, `image_text.rs` |
| metadata / EXIF / details | `meta.rs`, `media_details.rs` |
| timing, metrics, perf episodes | `timing.rs`, `metrics.rs`, `perf.rs` |
| secrets | `secret.rs` |
| the shell contract | `contract.rs`, `action.rs`, `engine.rs` |

**Test:** does the function read and write *that subsystem's* state more than `AppCore`'s? Then
it is a method on the subsystem, not on `AppCore`.

### 3. Is it a new subsystem — owned state plus behaviour over it?

→ **A new module**, and see *The two-halves rule* below.

**Test — subsystem or topic?** A *subsystem* owns state and can answer questions about it
(`VideoSession`, `ResidentRing`, `BackgroundOps`). A *topic* is just a group of related
`AppCore` methods with no state of their own (applying settings, drawing panels). Subsystems get
a module; topics get an `app_core_impl/` concern file.

### 4. Is it genuinely orchestration — coordinating several subsystems?

→ **`app_core_impl/<concern>.rs`**: a separate `impl AppCore` block in the concern's file.
Rust lets an inherent impl span modules in one crate, so this costs nothing.

→ **`app_core_impl.rs` itself only if it matches that file's charter** (below). If it doesn't,
it doesn't go there.

### 5. Is it platform I/O — a window, a menu, a native dialog, an OS API?

→ **The shell** (`pb-app` for winit, `pb-mac-ffi` + `mac/` for macOS).

⚠ **But split policy from mechanism first.** If the *other* shell will need the same behaviour,
the decision-making belongs in the core and only the realization belongs in the shell. This is
the #126 lesson, and it is the difference between writing a feature once and writing it twice:

- **Core:** when to show it, what it says, what cancels it, which operation it belongs to.
- **Shell:** actually creating the window / menu item / sheet.

If you are about to write a function in `pb-app` and can imagine `pb-mac-ffi` needing the same
one — stop and put its policy in the core.

---

## The two-halves rule (this is the anti-regrowth mechanism)

**When you add a subsystem module, create its `app_core_impl/` concern file in the same commit,
even if it starts nearly empty.**

That is precisely what #126 missed. `dir_scan.rs` existed; `app_core_impl/dir_scan.rs` did not,
so `arm_dir_scan`, `poll_dir_scan` and `cancel_dir_scan` went to the kitchen sink by default.

Two halves, always:

- `<name>.rs` — the types, the state, the pure logic. Unit-testable without `AppCore`.
- `app_core_impl/<name>.rs` — the `impl AppCore` methods that drive it from orchestration.

We do not rely on a line-count lint to prevent regrowth. **The structure is the mechanism**:
give every likely place for growth somewhere proper to live and it will not pile up in one file.
A guard would only measure the symptom, and the cheapest response to a failing threshold is to
raise the threshold.

---

## The charter for `app_core_impl.rs`

> **`app_core_impl.rs` holds the `AppCore` lifecycle, the event/action dispatch, and the
> residency & present engine. Nothing else.**

That sentence is the point. A method that does not fit it **visibly needs a home somewhere
else** — which turns "where does this go?" from a default into a question with an answer.

The file is mid-split (task #125) and still contains more than its charter. That is a known
in-progress state, **not licence to add to it.** If your method does not match the charter, it
does not go there, regardless of what its current neighbours look like.

---

## Smells — you are probably about to put it in the wrong place

- **"I'll add it next to the similar one."** Check whether the similar one is in the right place.
  This is exactly how #126's methods landed in the kitchen sink: the neighbours were there.
- **"It needs `&mut self` on `AppCore`."** Often true of orchestration, but ask *which* fields.
  If it touches one subsystem's state plus one flag, it is a subsystem method and the flag is
  the argument.
- **"It's just a small helper."** Small helpers are how a file reaches 22,000 lines. A pure
  helper is a free function in the module it serves.
- **"It's temporary."** It isn't.
- **"There's nowhere obvious."** That is the signal to *make* somewhere, not to default. If it
  genuinely fits no concern, say so in the commit message — a method that resists naming is
  either cross-cutting (and belongs to dispatch) or badly named (and wants renaming).

---

## Where the boundary is genuinely unclear

Some coupling in this codebase is **essential, not accidental**. Prefetch decisions really do
depend on view mode, zoom, fit box, playback state and archive scope — that *is* the Prime
Directive ("which interaction just became likely"). Do not force a narrow API onto genuinely
cross-cutting state; you will pay in plumbing on a per-frame path and gain nothing a user sees.

The honest position: **the residency/present policy is the one place where "it lives on
`AppCore`" is currently the right answer**, and moving it needs evidence — a bug it caused, or a
change it made hard — not architectural tidiness. Everything else has a better home.

---

## See also

- `.taskmaster/plans/125-split-app-core-impl.md` — the split in progress, its cluster inventory,
  and §9 on why anti-regrowth is structural rather than a lint.
- `.taskmaster/plans/126-dry-the-shell-orchestration.md` — the worked example of policy-in-core /
  mechanism-in-shell, and where the two-halves rule comes from.
- `CLAUDE.md` → *Cross-platform discipline* — the platform priority and the two-machine rules.
