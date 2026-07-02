# pb-mac-ffi — the macOS Swift/AppKit FFI bridge (NS1)

A [`swift-bridge`](https://github.com/chinedufn/swift-bridge) **staticlib** that exposes
PhotoBlaze's platform-neutral [`pb_app_core::AppCore`] to a SwiftUI/AppKit host (ADR-021,
NS1). It is the macOS analog of the winit shell in `crates/pb-app` — the shell translates
native input into `CoreEvent`s and executes the `CoreEffect`s the core returns; this crate
is the same seam, spoken over FFI. The host lives in **`mac/`** (a SwiftPM executable).

**macOS-only.** The `swift-bridge` dependency and the whole `ffi` module are target-gated,
so on Windows/Linux the crate compiles to an empty staticlib and the winit `pb-app` build is
untouched (the `--workspace` CI on those platforms is unaffected).

## The boundary

`AppCoreHandle` is an opaque handle the Swift host owns. Events go **in** as method calls;
effects come **out** of a pull-style drain the host runs **on the main actor**.

```
Swift host                         Rust (pb-mac-ffi → pb-app-core)
──────────                         ──────────────────────────────
NSEvent keyDown  ─ key_down() ───► AppCore::handle(CoreEvent::KeyDown{..})
MTKView draw     ─ tick() ───────► AppCore::handle(CoreEvent::Tick(now))
                 ◄─ while let e = next_effect() ── CoreEffectFfi  (render / title / wake / …)
```

Current FFI surface (grows per NS1 slice):

| Swift call | Core event / effect |
|---|---|
| `AppCoreHandle(width:height:scale:)` | `AppCore::headless(Viewport)` |
| `key_down(key:ctrl:shift:alt:logo:is_repeat:)` | `CoreEvent::KeyDown` |
| `key_up(key:)` | `CoreEvent::KeyUp` |
| `focus_lost()` | `CoreEvent::FocusLost` |
| `tick()` | `CoreEvent::Tick(now)` |
| `next_effect() -> CoreEffectFfi?` | pulls one queued effect (loop until `nil`) |

`key` is a **`PbKey` name accepted by `PbKey::from_name`** — `"Space"`, `"Escape"`,
`"Left"`/`"Right"`/`"Up"`/`"Down"`, `"Return"`, `"Backspace"`, single letters/digits
(`"C"`, `"9"`), … — **not** winit's `"ArrowRight"`/`"KeyC"` spellings. The Swift host maps
`NSEvent.keyCode` → that name (the input-adapter job, NS1 item 4). Unknown names are ignored.

**Quit semantics:** Esc resolves through the keymap to `Action::Quit`, a *host-side flow
command* — it arrives as `CoreEffectFfi::ShellFlowAction("quit")` (**not** `.Quit`); the
host runs the native teardown (`NSApp.terminate`). Same for `"delete_permanent"`,
`"recursive"`, `"cancel_scan"`.

## swift-bridge gotchas (each cost a debugging round — keep the list current)

1. **No `///` doc comments inside the `#[swift_bridge::bridge]` module** — they become
   `#[doc]` attributes that swift-bridge-ir's parser rejects (panics in build.rs codegen).
   Use `//` line comments.
2. **Crate-level `#![allow(clippy::unnecessary_cast)]`** — the generated `extern "C"` shims
   contain same-type pointer casts we can't edit.
3. **`Vec<transparent enum>` doesn't fully work**: 0.1.59 generates the *Rust* half of a
   `-> Vec<CoreEffectFfi>` return but not the Swift-side `Vectorizable` conformance or the
   `Vec_…` C shims — the generated *Swift* doesn't compile (and `cargo test` can't catch it;
   only `swift build` does). `Option<transparent enum>` is fully supported → the drain is
   pull-style (`next_effect`).
4. **Don't name a bridge parameter after a Swift keyword** (`repeat`, `where`, `in`, …) —
   the generated Swift call site uses the bare identifier without backticks and doesn't
   compile. Hence `is_repeat`.

## Building

The Rust side of the bridge is produced by the `#[swift_bridge::bridge]` proc-macro at
compile time; `build.rs` writes the *Swift-facing* glue to `generated/` on every
`cargo build` (git-ignored). The `create-package` bin (feature `package`) then wraps the
**built** staticlib + that glue into a local Swift package at `PbMacFfi/` (git-ignored) —
an `RustXcframework.xcframework` binary target + the generated `.swift` sources — which
`mac/Package.swift` consumes as a path dependency.

```sh
scripts/build-swift-host.sh [--debug|--release] [--run]   # the whole chain + .app
```

or by hand:

```sh
cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin
cargo run  -p pb-mac-ffi --features package --bin create-package   # → PbMacFfi/
swift build --package-path mac -c release
cargo test -p pb-mac-ffi                                           # round-trip proof
```

Frameworks: a staticlib carries **no framework references**, so `mac/Package.swift` links
what the Rust graph `#[link]`s (ImageIO / CoreGraphics / AVFoundation / CoreMedia / …) —
mirror `crates/pb-decode/src/{imageio,livephoto}.rs` when those change.

**Main-thread rule:** all `AppCoreHandle` calls + the `next_effect()` drain loop run on the
main actor. A Rust worker thread may only *schedule* a main-thread drain; it must never
touch AppKit/SwiftUI or the renderer directly.

Xcode IDE: `open mac/Package.swift` gives the full IDE/debugger against the same package
(build the Rust side first so `PbMacFfi/` exists).
