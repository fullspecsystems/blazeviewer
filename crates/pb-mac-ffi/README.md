# pb-mac-ffi — the macOS Swift/AppKit FFI bridge (NS1)

A [`swift-bridge`](https://github.com/chinedufn/swift-bridge) **staticlib** that exposes
PhotoBlaze's platform-neutral [`pb_app_core::AppCore`] to a SwiftUI/AppKit host (ADR-021,
NS1). It is the macOS analog of the winit shell in `crates/pb-app` — the shell translates
native input into `CoreEvent`s and executes the `CoreEffect`s the core returns; this crate
is the same seam, spoken over FFI.

**macOS-only.** The `swift-bridge` dependency and the whole `ffi` module are target-gated,
so on Windows/Linux the crate compiles to an empty staticlib and the winit `pb-app` build is
untouched (the `--workspace` CI on those platforms is unaffected).

## The boundary

`AppCoreHandle` is an opaque handle the Swift host owns. Events go **in** as method calls;
effects come **out** of a drain the host runs **on the main actor**.

```
Swift host                         Rust (pb-mac-ffi → pb-app-core)
──────────                         ──────────────────────────────
NSEvent keyDown  ─ key_down() ───► AppCore::handle(CoreEvent::KeyDown{..})
MTKView draw     ─ tick() ───────► AppCore::handle(CoreEvent::Tick(now))
                 ◄─ drain_effects() ── Vec<CoreEffectFfi>  (render / title / wake / quit / …)
```

Current FFI surface (grows per NS1 slice):

| Swift call | Core event |
|---|---|
| `AppCoreHandle(width:height:scale:)` | `AppCore::headless(Viewport)` |
| `key_down(key:ctrl:shift:alt:logo:repeat:)` | `CoreEvent::KeyDown` |
| `key_up(key:)` | `CoreEvent::KeyUp` |
| `focus_lost()` | `CoreEvent::FocusLost` |
| `tick()` | `CoreEvent::Tick(now)` |
| `drain_effects() -> [CoreEffectFfi]` | drains `AppCore.effects` |

`key` is a **`PbKey` name** (e.g. `"Space"`, `"ArrowRight"`, `"KeyC"` — see `PbKey::as_str`);
the Swift host maps `NSEvent` → that name (the input-adapter job). Unknown names are ignored.

## Building

The Rust side is produced by the `#[swift_bridge::bridge]` proc-macro at compile time; the
Swift-facing glue is written by `build.rs` (via `swift-bridge-build`) to `generated/` on every
`cargo build` (git-ignored — it's a build artifact).

```sh
cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin   # what Xcode runs
cargo test  -p pb-mac-ffi                                           # round-trip proof
```

After a build, `generated/` contains:
- `pb-mac-ffi/pb-mac-ffi.swift` — the generated `AppCoreHandle` + `CoreEffectFfi` Swift types
- `SwiftBridgeCore.swift` / `SwiftBridgeCore.h` — the swift-bridge runtime glue

## Xcode integration (NS1, forthcoming)

An Xcode app target (arm64-only, min macOS 14 — ADR-021) will:
1. Add a **Run Script build phase** that runs `cargo build -p pb-mac-ffi --release
   --target aarch64-apple-darwin`.
2. Link `target/aarch64-apple-darwin/release/libpb_mac_ffi.a`.
3. Add the `generated/` `.swift` files to the target and expose the `.h` via a bridging
   header / module map.
4. Own the `NSWindow` + `MTKView`; hand the retained `CAMetalLayer` (`*mut c_void`) to a
   macOS-only wgpu surface constructor (`SurfaceTargetUnsafe::CoreAnimationLayer`) — the next
   slice.

**Main-thread rule:** all `AppCoreHandle` calls + `drain_effects()` run on the main actor.
A Rust worker thread may only *schedule* a main-thread drain; it must never touch
AppKit/SwiftUI or the renderer directly.
