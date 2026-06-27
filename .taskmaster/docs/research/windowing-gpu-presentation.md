# Windowing, GPU API & Low-Latency Presentation

Research for **PhotoBlaze** — a Rust photo viewer obsessed with keypress -> photon latency.
Target: Windows 11 (primary), future macOS (Apple M-series). Display: 7680x3840 @ 120Hz. GPU: RTX 5090.

Research date: 2026-06-26. Versions referenced are current as of mid-2026 (winit 0.30.x, wgpu v29.0.1).

---

## Overview

PhotoBlaze's hot path is: a key is held (or tapped) -> advance to the newest decoded-and-uploaded photo -> present it within one display refresh (8.33 ms at 120 Hz). The architecture we want is an **own auto-repeat tied to the render loop**: we ignore the OS key-repeat stream entirely, track raw key-down/key-up state, and on each vblank advance to the newest "ready" frame, capped to the monitor refresh.

This shapes every choice below:

- **Windowing/input (winit):** we need clean key-down / key-up and to *discard* OS auto-repeat. winit gives us exactly the bit we need (`KeyEvent.repeat`), and the well-known Windows key-repeat lag bug is irrelevant *because we ignore repeats*. The real risk is a lost key-up on Windows (winit issue #4233).
- **Presentation:** the lowest, most consistent keypress-to-photon latency on Windows comes from the DXGI flip model with a frame-latency **waitable object** (+ optionally `ALLOW_TEARING`). wgpu's D3D12 backend now exposes enough of this to be viable; raw D3D12 is the ceiling.
- **GPU API:** **wgpu** (D3D12 on Windows, Metal on macOS) is the recommended primary, because the macOS port is nearly free and wgpu's D3D12 backend recently gained the latency knobs that mattered. The one place wgpu genuinely limits us is **asynchronous texture upload** — there is no exposed async copy/transfer queue, which matters for streaming 100+ MB images.

The biggest single technical risk is not presentation latency (well-trodden) but **getting a 118-236 MB image onto the GPU without stalling the keypress frame** — that is an upload/prefetch problem, and wgpu's single-queue model is the constraint.

---

## winit & input / key-repeat

### Current state (2025-2026)

- Latest stable is **winit 0.30.x** (0.30.12 at time of writing). Winit 0.30 overhauled the event loop: everything now goes through the **`ApplicationHandler`** trait, and you call `EventLoop::run_app` (the old `EventLoop::run` / closure model is deprecated). Windows must now be created **inside the running loop** (typically in `resumed()` or `new_events(StartCause::Init)`), not before launch. ([winit 0.30 changelog](https://rust-windowing.github.io/winit/winit/changelog/v0_30/index.html), [ApplicationHandler docs](https://docs.rs/winit/0.30.5/winit/application/trait.ApplicationHandler.html))
- The `master`/next (future 0.31) branch is mid-refactor: `user_event` became `user_wake_up` (the generic user-event payload was removed; `EventLoopProxy::send_event` -> `wake_up`), and the **monitor API is being reworked** (see refresh-rate section). Pin a winit version and budget for a migration. ([winit changelog discussion](https://github.com/rust-windowing/winit/releases))

### KeyEvent: the fields that matter

`WindowEvent::KeyboardInput` delivers a `KeyEvent` with ([KeyEvent docs](https://docs.rs/winit/latest/winit/event/struct.KeyEvent.html)):

| Field | Meaning | Use for PhotoBlaze |
|---|---|---|
| `physical_key` (`PhysicalKey`/`KeyCode`) | Layout-independent physical position | **Primary** — bind arrow/PageUp/Down/Home/End by physical position |
| `logical_key` (`Key`) | Layout/modifier-aware value | Shortcuts that should respect layout |
| `text` (`Option<SmolStr>`) | Produced text (e.g. Enter -> `"\r"`) | Not needed on hot path |
| `location` | Left vs right of duplicate keys | Modifier disambiguation only |
| `state` (`ElementState`) | `Pressed` / `Released` | **Core of our own repeat** |
| `repeat` (`bool`) | `true` iff this event is an OS auto-repeat | **We ignore events where this is `true`** |

The docs are explicit: "In games, you often want to ignore repeated key events — this can be done by ignoring events where this property [`repeat`] is set." That is precisely our model.

### Implementing our own auto-repeat (recommended pattern)

1. On `KeyboardInput`, **drop any event with `repeat == true`**.
2. Maintain a small `HashSet`/bitset of currently-held `physical_key`s: insert on `Pressed { repeat:false }`, remove on `Released`.
3. In the render loop, once per vblank, if a navigation key is held, advance to the newest decoded+uploaded photo (capped to refresh — see refresh-rate section). Optionally apply an initial-delay-then-accelerate curve in *your* timebase, fully decoupled from OS repeat rate.

This sidesteps the notorious Windows key-repeat lag bug entirely:

> **winit issue #4043** — on Windows (winit 0.29.x) the second event (`state=Pressed, repeat=true`) lags the initial press by **20-30 frames at 60 FPS**, and some repeats are dropped, while the initial press and the release are near-instant. ([issue #4043](https://github.com/rust-windowing/winit/issues/4043))

Because we never consume `repeat==true` events, this lag does not touch us — our repeat cadence comes from the render loop, and `Pressed`/`Released` (the events we *do* use) are reported promptly.

### The real caveat: lost key-up on Windows (issue #4233)

winit on Windows derives physical key events from **WM_ window messages**, which can **miss some physical key transitions**. The filed example: in a `LeftShift-down, RightShift-down, RightShift-up, LeftShift-up` sequence, the `RightShift` key-up is **never reported** via `WindowEvent::KeyboardInput`, but **is** visible in Windows **Raw Input**. The issue (open, B-bug/DS-win32, May 2025) proposes switching the Windows backend to Raw Input. ([issue #4233](https://github.com/rust-windowing/winit/issues/4233))

Impact on us: if a key-up is dropped, our held-set keeps the key "held" and we advance forever. Mitigations:

- Treat **focus loss / `WindowEvent::Focused(false)`** as "release all held keys."
- Add a cheap **safety net**: either subscribe to `DeviceEvent::Key` (raw `RawKeyEvent`: `physical_key` + `state`, no `repeat`, no `text`, delivered regardless of focus) and reconcile, or poll `GetAsyncKeyState` for the handful of nav keys each vblank to correct the held-set. For arrow-key navigation specifically the lost-up case is rare (it centers on modifier interleaving), so a lightweight reconcile is enough.

`DeviceEvent`/`RawKeyEvent` is the lower-level source but lacks repeat/text and is harder to map to focus; use `WindowEvent::KeyboardInput` as primary and raw as a corrective net.

### Borderless fullscreen with no chrome

- `Window::set_fullscreen(Some(Fullscreen::Borderless(None)))` fullscreens on the current monitor with no mode switch; combine with `WindowAttributes::with_decorations(false)`. ([Fullscreen docs](https://docs.rs/winit/latest/winit/window/enum.Fullscreen.html)) Fullscreen and mobile windows have no decorations by definition.
- Prefer **Borderless** over **Exclusive** fullscreen: borderless keeps instant alt-tab, plays well with DWM, and is the configuration that the DXGI/Metal compositor can promote to **Independent Flip / Direct Flip** (fullscreen-equivalent latency in a window) when the swapchain matches the screen at native resolution. Exclusive mode buys little on modern Windows and complicates multi-monitor and HDR.

### Alternatives to winit

| Option | Pros | Cons for PhotoBlaze |
|---|---|---|
| **winit 0.30** | Rust-native default; integrates with wgpu via `raw-window-handle`; cross-platform incl. macOS; gives `repeat` flag and raw `DeviceEvent` | Windows backend WM-message key-up gaps (#4233); monitor API churn on master |
| **SDL3** (`sdl3-rs` 0.18, wraps SDL 3.2.x) | Very mature input incl. raw scancodes and robust repeat handling; battle-tested cross-platform; also ships its own GPU API | Pulls in a C dependency; its GPU abstraction competes with wgpu; less idiomatic with the Rust GPU ecosystem ([sdl3-rs](https://github.com/vhspace/sdl3-rs), [docs.rs/sdl3](https://docs.rs/sdl3/latest/sdl3/)) |
| **Raw Win32** (`windows` crate) | Absolute control: Raw Input for perfect key state, hand-built DXGI swapchain, exact present timing | Windows-only (kills the macOS port from the shared layer); much more code; you re-implement everything winit gives free |

**Recommendation:** winit 0.30 primary, with a raw-input/`GetAsyncKeyState` safety net for the #4233 edge. SDL3 only if winit's input proves insufficient in testing; raw Win32 only behind a renderer abstraction as a Windows-only escape hatch.

---

## Refresh-rate detection

We need the monitor refresh to **cap advance speed** (advance at most one newest-ready frame per refresh).

### Stable winit 0.30.12 API (use this)

`MonitorHandle` exposes (confirmed signatures, winit 0.30.12 — [MonitorHandle docs](https://docs.rs/winit/0.30.12/winit/monitor/struct.MonitorHandle.html)):

```rust
monitor.refresh_rate_millihertz() -> Option<u32>   // current system refresh, in mHz
monitor.video_modes() -> impl Iterator<Item = VideoModeHandle>
monitor.size() -> PhysicalSize<u32>
monitor.scale_factor() -> f64
monitor.name() -> Option<String>
monitor.position() -> PhysicalPosition<i32>
```

- **mHz, not Hz**: a 120 Hz panel returns `Some(120000)` (or a near approximation like `119998`). Divide by 1000.
- Get the relevant monitor via `window.current_monitor()`, or enumerate `event_loop.available_monitors()` / `primary_monitor()`.
- Each `VideoModeHandle` carries `size()`, `bit_depth()`, and `refresh_rate_millihertz()` for enumerating supported fullscreen modes. ([VideoMode docs](https://docs.rs/winit/0.20.0-alpha4/winit/monitor/struct.VideoMode.html))
- **Note:** in 0.30.12 there is **no** `current_video_mode()` method — use `MonitorHandle::refresh_rate_millihertz()` for the *current* rate.

### Future winit (master / 0.31) — API change incoming

On the `master` docs, `MonitorHandle` is being refactored to `Deref<Target = dyn MonitorHandleProvider>` and a `current_video_mode()` accessor; `refresh_rate_millihertz()` moves under the provider/`VideoModeHandle`. ([master MonitorHandle docs](https://rust-windowing.github.io/winit/winit/monitor/struct.MonitorHandle.html)) Pin winit and isolate refresh-rate reads behind one helper so the migration is one function.

### Caveats for a latency app

- The value is an **integer approximation** and "should not be relied upon to be exact" — fine for computing an advance budget, **not** for exact frame pacing.
- In **borderless windowed** mode the *effective* present cadence is set by DWM/the swapchain at the desktop's current mode, which equals the monitor's set refresh — so `refresh_rate_millihertz()` is the right cap, but for true pacing prefer **present feedback** (DXGI `GetFrameStatistics` on Windows, drawable/`CADisplayLink` timing on macOS — see Raph Levien below).
- **VRR / G-Sync** on this panel makes the instantaneous rate variable; the reported value is the max. If we run vsync-off / allow-tearing under VRR, the "one advance per refresh" cap should use the max rate as an upper bound.

---

## GPU API comparison: wgpu vs ash (Vulkan) vs native D3D12

The render workload is trivial (one textured quad). The differentiators are **present latency control** and **texture-upload throughput**, plus **portability**.

### Decision table

| Criterion | **wgpu** (v29.0.1) | **ash** (raw Vulkan) | **native D3D12** (`windows` crate) |
|---|---|---|---|
| Language/safety | Safe Rust, single API | Unsafe FFI, manual sync/alloc | Unsafe FFI, manual everything |
| Backends | Vulkan / Metal / **D3D12** / GL / WebGPU | Vulkan only (Windows/Linux/Android; macOS via MoltenVK layer) | D3D12 only (Windows) |
| **macOS M-series** | **Yes, first-class (Metal)** | Only via MoltenVK (extra layer, not first-class) | **No** |
| Lowest Windows present latency | Good — D3D12 backend uses DXGI flip + waitable; can hand control to app (v27) | **Worse on Windows** — Vulkan swapchain lacks DXGI waitable/colorspace/frame-pacing tools (wgpu #8354) | **Best/ceiling** — full DXGI flip, waitable, ALLOW_TEARING, MPO, frame stats |
| Present modes | Fifo/FifoRelaxed/Immediate/Mailbox/Auto* | All Vulkan modes incl. MAILBOX/IMMEDIATE (NVIDIA OK) | Flip model + sync-interval-0/allow-tearing |
| **Async copy/transfer queue** | **Not exposed** (single queue) | **Yes** (dedicated transfer queue) | **Yes** (dedicated COPY queue) |
| HDR surface | scRGB/extended-sRGB color space landing in trunk (unreleased as of 29.0.1); Rgba16Float surface works on Metal today | Full control (manual) | Full control (manual) |
| Effort for our app | Low | High | High, Windows-only |
| One-textured-quad cost | Trivial | Trivial-but-verbose | Trivial-but-verbose |

### Why Vulkan/ash is the *wrong* low-latency pick on Windows

wgpu's own maintainers document that **DXGI-based swapchains beat Vulkan swapchains on Windows** for latency and correctness:

> "All of these problems [input latency, color space, resize artifacts, flicker] do not exist on DX12 where we're using a DXGI swapchain. ... The Vulkan backend lacks ... FrameLatencyWaitableObject [for] sane latency controls." — wgpu issue #8354 (Oct 2025), proposing **DXGI-swapchain interop for the Vulkan backend**. ([issue #8354](https://github.com/gfx-rs/wgpu/issues/8354))

So raw ash on Windows would inherit exactly the latency limitations wgpu is trying to engineer *around*. NVIDIA's Vulkan does support `MAILBOX` and `IMMEDIATE`, but you still lack the DXGI waitable object and per-swapchain `SetMaximumFrameLatency`.

### Verdict

**wgpu primary**, using its **D3D12 backend on Windows** and **Metal on macOS**, behind a thin `Renderer` trait that leaves a **native-D3D12 escape hatch**. Rationale:

- The macOS M-series port is nearly free (same Rust render code).
- Since v27, the wgpu D3D12 backend exposes the DXGI **frame-latency waitable object** and lets us **take manual control** of it (below) — the single most important latency knob.
- The only capability we genuinely lose vs raw D3D12 is the **async copy queue** for texture streaming. If profiling shows prefetch can't hide uploads, drop to a raw-D3D12 renderer behind the same trait — Windows-only, but that's where it matters.

---

## Present modes & low-latency presentation on Windows

### wgpu PresentMode semantics ([PresentMode docs](https://docs.rs/wgpu/latest/wgpu/enum.PresentMode.html))

| Mode | Behavior | Tearing | Backend support | a.k.a. |
|---|---|---|---|---|
| `Fifo` | ~3-frame FIFO queue, pop one per vblank | No | **All platforms** (default) | Vsync On |
| `FifoRelaxed` | FIFO, but late frame tears in immediately | If late | AMD on Vulkan | Adaptive Vsync |
| `Immediate` | No queue; swap to front on present | Yes | Most platforms except old DX12 / Wayland | Vsync Off |
| `Mailbox` | Single-slot queue; newest replaces queued | **No** | **DX12 on Win10+, NVIDIA on Vulkan**, Wayland/Vulkan | Fast Vsync |
| `AutoVsync` | First of {FifoRelaxed, Fifo} | No | everywhere (fallback) | — |
| `AutoNoVsync` | First of {Immediate, Mailbox, Fifo} | maybe | everywhere (fallback) | — |

For PhotoBlaze (mostly a static image until a keypress):

- **`Mailbox` is the sweet spot for tear-free lowest latency.** It is supported on **DX12/Win10+ and NVIDIA-on-Vulkan** — our RTX 5090 qualifies on both backends. We render the newest-ready photo and it goes up at the next vblank with no extra queued-frame latency, no tearing. Only the latest submission survives, so a fast key-scrub never shows a stale queued frame.
- **`Immediate` (vsync-off / allow-tearing)** is the absolute-minimum-latency option if a tear is acceptable. On a 120 Hz panel a tear line is short-lived (8.33 ms frames) and barely visible on a static photo; worth offering as a user toggle for "fastest."
- **Avoid plain `Fifo`** if latency matters: it can add up to ~3 frames of queue. wgpu added a **workaround for "extremely poor frame pacing from AMD and Nvidia cards on Windows in `Fifo`/`FifoRelaxed`"** (v27.0.4 and v26.0.6), because drivers implicitly back these with DXGI swapchains with different timing — so even Fifo behaves better now, but Mailbox/Immediate are still lower latency. ([wgpu CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md))

### The DXGI flip model and the waitable object (the core latency mechanism)

Under the hood on Windows you want the **DXGI flip model** (`DXGI_SWAP_EFFECT_FLIP_DISCARD`). The legacy blt model is deprecated ("if you are still using `DXGI_SWAP_EFFECT_DISCARD`/`SEQUENTIAL` ... it's time to stop!"). ([DXGI flip model devblog](https://devblogs.microsoft.com/directx/dxgi-flip-model/))

**Why blocking on `Present` adds latency:** when you call `Present`, the system blocks until a prior frame is done presenting, then queues yours — "the system will reach a stable equilibrium where the game is always waiting almost a full extra frame between the time it renders and the time it presents." The fix is the **frame-latency waitable object**: ([Reduce latency with DXGI 1.3](https://learn.microsoft.com/en-us/windows/uwp/gaming/reduce-latency-with-dxgi-1-3-swap-chains))

1. Create the swapchain with `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`.
2. `IDXGISwapChain2::SetMaximumFrameLatency(1)` (default for waitable swapchains is **1** = least latency; use 2 only if you need more CPU-GPU overlap to hit framerate).
3. `GetFrameLatencyWaitableObject()` -> a handle.
4. **`WaitForSingleObjectEx(handle, ...)` at the *top* of the loop, before processing input and rendering** — including before the first Present.

This inverts the flow: instead of "render, then block in Present," you "**wait until the system can accept a frame, then sample the latest input and render**." That removes the ~1 stale frame and is exactly aligned with our "advance to newest ready frame each vsync" design. Raph Levien calls blocking-on-present "the old way" that "gives particularly bad results"; the modern approach schedules rendering against the real present deadline. ([Raph Levien, Swapchains and frame pacing](https://raphlinus.github.io/ui/graphics/gpu/2021/10/22/swapchain-frame-pacing.html))

### Even lower: ALLOW_TEARING + Independent Flip

- **`DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING`** + **`DXGI_PRESENT_ALLOW_TEARING`** with **sync interval 0** gives latency *below* the waitable object, "even in a window on systems with multi-plane overlay support." Requires checking `IDXGIFactory5::CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING)`. ([Variable refresh rate displays](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/variable-refresh-rate-displays))
- **Independent Flip / Direct Flip + MPO:** when a borderless-fullscreen swapchain matches the screen at native resolution, DWM steps out and "your app frames are sent directly to screen, independently, with the same efficiency as fullscreen exclusive." This is the path to true ~1-frame latency in a window. Down to **1 frame of latency** is achievable on recent Windows in Independent Flip with the waitable object. ([DXGI flip model devblog](https://devblogs.microsoft.com/directx/dxgi-flip-model/))
- NVIDIA's guidance agrees: flip-model swapchains, leverage MPO, `SetMaximumFrameLatency` + waitable flag to override the default 3-frame queue, and 1-2 more buffers than max queued frames. ([NVIDIA, Advanced API Performance: Swap Chains](https://developer.nvidia.com/blog/advanced-api-performance-swap-chains/))

### What wgpu gives us of all this

- **v27.0.0**: "Allow disabling waiting for latency waitable object" via **`Dx12BackendOptions`** (`use_latency_waitable_object`-style flag). The swapchain is still created with the waitable flag, and wgpu **exposes the waitable handle** so the app can wait at its own optimal point instead of wgpu waiting internally. This is the hook that lets us implement the "wait at top of loop -> sample input -> render latest" pattern through wgpu. ([wgpu PR #7400](https://github.com/gfx-rs/wgpu/pull/7400), [CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md))
- **v27.0.4 / v26.0.6**: AMD/NVIDIA Fifo/FifoRelaxed frame-pacing workaround (above).
- **v28.0.1**: fixed an NVIDIA crash when **presenting from another thread** — relevant if we present off the input thread.
- What wgpu does **not** cleanly expose yet: `ALLOW_TEARING` is reachable only indirectly via `Immediate` present mode; fine-grained MPO/Independent-Flip control and DXGI `GetFrameStatistics` feedback are not surfaced. For those, a raw-D3D12 backend is the escape hatch.

**Lowest, most consistent keypress-to-photon without tearing:** `Mailbox` present mode + the DX12 waitable object (app-controlled wait) + borderless fullscreen at native res to engage Independent Flip. For absolute minimum accepting a faint tear: `Immediate`/allow-tearing + sync interval 0.

---

## wgpu texture-upload reality

This is where wgpu most constrains a latency-obsessed image viewer. A full-res frame is large: 7680x3840 RGBA8 = **~118 MB**; RGBA16F = **~236 MB**. Getting that onto the GPU without stalling the keypress frame is the hard part.

### `queue.write_texture` — what actually happens ([Queue docs](https://docs.rs/wgpu/latest/wgpu/struct.Queue.html))

- The data is **copied immediately into staging memory** (you may discard your `&[u8]` right after the call), but the GPU copy **executes only on the next `Queue::submit()`** — `write_buffer`/`write_texture` "do _not_ submit the transfer to the GPU immediately."
- **Per-call allocation:** "Currently on native platforms ... the staging memory will be a **new allocation**. This will then be **released after the next submission finishes**." So every `write_texture` of a 118 MB image allocates and frees 118 MB of staging — **per-frame allocation churn** plus an extra CPU memcpy into staging.
- Real-world symptom (wgpu discussion #5899, an image gallery): preloading 60 textures stalled "20 seconds or more"; profiling showed time-spikes **specifically on `write_texture` calls**. ([discussion #5899](https://github.com/gfx-rs/wgpu/discussions/5899))

### Faster paths within wgpu

- **`StagingBelt`** (`wgpu::util`): a ring of sub-allocated, **reused** staging buffers — no per-call allocation. For textures you write into the belt's buffer and then `copy_buffer_to_texture` (the belt itself only writes buffers). v27 changed it to take the `Device` at creation; v29 adds `finish_and_recall_on_submit`. Best when you have many uploads; for one big image per advance it amortizes allocation but still goes through the graphics timeline. ([StagingBelt docs](https://docs.rs/wgpu/latest/wgpu/util/struct.StagingBelt.html), [CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md))
- **Roll your own persistent upload buffers**: create a small pool of mapped `COPY_SRC` buffers, write the decoded image into a free one, `copy_buffer_to_texture` into a destination texture. No churn, full control over which submit it lands on, and you can **double-buffer the destination textures** so the visible image is never the one being written.

### The hard limit: no async copy/transfer queue

wgpu/WebGPU exposes a **single `Queue`**. There is **no public async transfer/copy queue**, so uploads are ordered on the **same timeline** as rendering. The driver may still DMA in parallel, but you cannot, in safe wgpu, run an independent copy queue that overlaps the graphics queue the way raw Vulkan (transfer queue) or D3D12 (COPY queue) allow. wgpu maintainers have discussed asking Vulkan for a separate transfer queue internally ("being able to overlap drawing and transfer is a big win"), but it is **not surfaced to the application**. ([StagingBelt textures issue #1444](https://github.com/gfx-rs/wgpu/issues/1444))

**This is the one place wgpu blocks the fastest upload path.** For a viewer that wants to stream the *next* image onto the GPU on a copy queue while the *current* frame renders, raw D3D12's COPY queue / Vulkan's transfer queue is strictly more capable.

### Quantification and mitigation

- PCIe 5.0 x16 is ~**64 GB/s** theoretical; a 118 MB texture is therefore ~2 ms of pure transfer at best, realistically a few ms with overhead — **too long to do on the 8.33 ms keypress frame** alongside everything else, especially with the extra staging memcpy.
- **Never `write_texture` a full image on the keypress frame.** Instead:
  - **Prefetch** next/previous images: decode + upload ahead of time during idle vblanks, into a **pool of pre-allocated destination textures** (e.g. an LRU ring of N full-res textures).
  - Upload from **persistent mapped staging buffers** + `copy_buffer_to_texture`, not `write_texture`.
  - On a keypress, the "advance" is then just **swapping which already-resident texture the quad samples** — effectively free.
- If scrubbing faster than prefetch can keep up (holding the key, advancing every 8.33 ms through uncached images), the single-queue model becomes the bottleneck — that is the scenario that would justify the raw-D3D12 copy-queue escape hatch.

---

## HDR / wide-gamut output (swapchain/display path)

Goal: drive the display in HDR/wide gamut (scRGB or HDR10 on Windows, EDR/Display-P3 on macOS), doing the final color transform in a shader. (Decode-side color is another agent's scope.)

### Released wgpu (<= v29.0.1): float surfaces limited, tonemap pattern

- wgpu has **not** generally allowed a float surface format (`Rgba16Float`) on most backends; the established pattern (learn-wgpu) is: render to an **offscreen `Rgba16Float`** target, then **tonemap/encode in a shader** and present to a supported unorm surface (`Bgra8UnormSrgb`, `Rgba8UnormSrgb`, `Rgb10a2Unorm`). ([learn-wgpu HDR tutorial](https://sotrh.github.io/learn-wgpu/intermediate/tutorial13-hdr/))
- **Exception — Metal:** a `SurfaceConfiguration` with `TextureFormat::Rgba16Float` *does* work on macOS/Metal and lets you output values > 1.0, **activating EDR/HDR** on Apple displays. ([wgpu issue #2920](https://github.com/gfx-rs/wgpu/issues/2920))

### New (wgpu trunk, unreleased as of 29.0.1): explicit surface color space

The CHANGELOG "Unreleased" section adds: *"Surfaces can now be configured with an explicit color space, enabling HDR and wide-gamut output where the platform supports it,"* via **`SurfaceColorSpace::ExtendedSrgb`** (extended-range nonlinear sRGB) and **`SurfaceColorSpace::ExtendedSrgbLinear`** (**scRGB**), across multiple backends. ([wgpu CHANGELOG](https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md)) This is the feature to track for first-class Windows HDR through wgpu — it is **in trunk but not in a release yet** (latest release is v29.0.1, 2026-03-26).

This mirrors the WebGPU direction: **`WGPUSurfaceColorManagement`** extends `WGPUSurfaceConfiguration` with `colorSpace` (`WGPUPredefinedColorSpace` — sRGB / Display-P3) and `toneMappingMode` (`standard` / `extended`). ([WebGPU-native header](https://webgpu-native.github.io/webgpu-headers/structWGPUSurfaceColorManagement.html)) Chrome 129 already ships `rgba16float` surfaces with `extended` tone mapping. ([Chrome 129 WebGPU](https://developer.chrome.com/blog/new-in-webgpu-129)) wgpu also exposes `PredefinedColorSpace` (srgb / display-p3) on the WebGPU API surface.

### Windows specifics (what the swapchain needs)

- **scRGB HDR:** surface format `DXGI_FORMAT_R16G16B16A16_FLOAT` + color space `DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709`. Linear scRGB where **1.0 = 80 nits, 12.5 = 1000 nits**. ([MS DirectX HDR](https://learn.microsoft.com/en-us/windows/win32/direct3darticles/high-dynamic-range))
- **HDR10:** `DXGI_FORMAT_R10G10B10A2_UNORM` + `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (PQ / Rec.2020). Plus `IDXGISwapChain4::SetHDRMetaData`.
- The **gamut map + PQ-or-scRGB encode happens in our shader**; the swapchain just needs the right format + color space.

### macOS EDR equivalent

- `CAMetalLayer` with `Rgba16Float` + `wantsExtendedDynamicRangeContent` and a Display-P3 colorspace; values > 1.0 use the EDR headroom. wgpu's Metal backend already supports `Rgba16Float` surfaces, so EDR is the **easiest HDR path available today** through wgpu.

### Practical recommendation

- Do **all color management in the shader** (P3/Rec.2020 primaries, PQ or scRGB encode) regardless of backend, writing to whatever surface format + color space the platform exposes.
- **Today (released wgpu):** EDR works on Metal via `Rgba16Float`; on Windows, robust scRGB/HDR10 output either waits for the trunk `SurfaceColorSpace` feature to ship, or uses a **raw-D3D12 swapchain** with the float/10-bit format + color space set manually. Ship SDR `Rgb10a2Unorm`/`Bgra8UnormSrgb` first; light up Windows HDR when `SurfaceColorSpace::ExtendedSrgb(Linear)` lands in a wgpu release.

---

## macOS portability & the abstraction boundary

### What wgpu buys the M-series port

For "one textured quad + texture uploads + present," wgpu makes the macOS port **nearly free**: the same WGSL shader and Rust render code run on Metal. The only genuinely divergent pieces are **surface/color-space configuration** and **present-latency tuning** — small, well-contained surfaces.

Raw D3D12 would lock the renderer to Windows; macOS would need a **second, hand-written Metal renderer** (via `objc2`/`metal` crates + `CAMetalLayer`). That is a full parallel backend, not a tweak.

### The portable abstraction boundary

Define a `Renderer` trait that owns the three platform-divergent concerns and nothing else:

1. **Surface/swapchain config** — present mode, buffer count, color space/format, and the latency-wait hook (DXGI waitable handle on Windows / drawable timing on macOS).
2. **Texture upload/prefetch** — the staging-buffer pool + destination-texture ring (so a raw-D3D12 copy-queue implementation can slot in on Windows without touching call sites).
3. **Submit + present timing** — "wait for present-ready -> sample input -> render newest -> present."

Implement this **once over wgpu**. Keep the seams clean enough that a **native-D3D12 implementation** of the same trait can replace it on Windows if latency/upload profiling demands it, without disturbing the macOS path.

### Metal present / CAMetalLayer latency notes ([Daniel Hooper, low-latency macOS](https://x.com/DanielcHooper/status/2067320915545686158), [displaySyncEnabled docs](https://developer.apple.com/documentation/quartzcore/cametallayer/displaysyncenabled))

For lowest latency on macOS:
- `layer.maximumDrawableCount = 2` — "pretty much guaranteed the frame will be displayed on the next vsync if rendering completes quickly enough." With `3`, CPU-to-display latency varies widely (~16 ms up to ~50 ms).
- `layer.presentsWithTransaction = YES` — synchronous present.
- `layer.displaySyncEnabled = window_is_fullscreen` — vsync tied to fullscreen state.
- `layer.opaque = YES`.
- `mach_wait_until(...)` to delay rendering to the last possible moment before the deadline (the macOS analog of the DXGI waitable wait-then-render).

macOS has **no exact DXGI-waitable equivalent**; the analog is `maximumDrawableCount = 2` + drawable/`CADisplayLink` present-time feedback + `mach_wait_until`. wgpu sets some Metal layer properties internally; squeezing the last bit may need direct `metal-rs`/`objc2` access to the `CAMetalLayer` behind the wgpu surface.

---

## Recommendations

1. **Windowing:** winit 0.30.x, `ApplicationHandler`/`run_app`. Borderless fullscreen on `current_monitor()` via `Fullscreen::Borderless(None)`, decorations off, at the display's native resolution (engages Independent Flip).
2. **Own auto-repeat:** drop all `KeyEvent { repeat: true }`; track held physical keys from `Pressed`/`Released`; advance one newest-ready photo per vblank in our timebase. This makes the Windows key-repeat-lag bug (#4043) irrelevant.
3. **Lost-key-up safety net (#4233):** release-all on focus loss; reconcile held-set via `DeviceEvent::Key` (raw) or per-vblank `GetAsyncKeyState` for the nav keys.
4. **Refresh rate:** `monitor.refresh_rate_millihertz()` (mHz; 120 Hz -> ~120000) to cap advance speed; isolate behind one helper (master winit changes this API). Don't use it for exact pacing — use present feedback.
5. **GPU API:** wgpu primary — **D3D12 backend on Windows, Metal on macOS**. Pin **wgpu >= v27** (latency-waitable control + Fifo frame-pacing fixes); track **>= the release that ships `SurfaceColorSpace`** for HDR. Keep a `Renderer` trait with a native-D3D12 escape hatch.
6. **Present mode:** **`Mailbox`** for tear-free lowest latency (RTX 5090 supports it on DX12 and NVIDIA-Vulkan). Offer **`Immediate`** (vsync-off/allow-tearing) as a "fastest" toggle. Avoid plain `Fifo` for the hot path.
7. **Latency mechanism:** use the DX12 **frame-latency waitable object** — disable wgpu's internal wait (`Dx12BackendOptions`, v27), grab the handle, and `WaitForSingleObjectEx` at the **top of the loop** so we sample input *after* the wait and render the newest frame. `SetMaximumFrameLatency(1)`.
8. **Texture upload:** never `write_texture` a full image on the keypress frame. **Prefetch** decode+upload of neighbor images during idle vblanks into a **ring of pre-allocated destination textures**, fed by **persistent mapped staging buffers + `copy_buffer_to_texture`** (or `StagingBelt`). On keypress, just rebind the resident texture. Accept that wgpu has **no async copy queue**; if prefetch can't hide uploads while scrubbing, use raw D3D12's COPY queue behind the trait.
9. **HDR:** color-manage in the shader (P3/Rec.2020 + PQ/scRGB). Ship SDR (`Rgb10a2Unorm`) first. EDR works today on Metal via `Rgba16Float`; light up Windows scRGB/HDR10 when wgpu's `SurfaceColorSpace::ExtendedSrgb(Linear)` ships (or via a raw-D3D12 swapchain meanwhile).

---

## Open Questions

1. **Does wgpu's D3D12 backend actually engage Independent Flip / MPO** with a borderless-fullscreen swapchain at 7680x3840@120, or does DWM still compose (adding a frame)? Needs **PresentMon** measurement on the real rig.
2. **Measured latency delta:** wgpu `Mailbox` vs hand-rolled raw-D3D12 (waitable + `ALLOW_TEARING` + sync-interval-0) on the 120 Hz panel — keypress-to-photon via PresentMon / a hardware latency tool (LDAT). Is wgpu within an acceptable margin?
3. **Upload throughput while scrubbing:** can we prefetch + upload 118-236 MB images fast enough to advance every 8.33 ms through uncached photos? Is wgpu's single-queue model the bottleneck, and does the raw-D3D12 COPY-queue escape hatch close the gap?
4. **HDR timeline:** which wgpu release ships `SurfaceColorSpace::ExtendedSrgb`/`ExtendedSrgbLinear`, and does the D3D12 backend wire up the scRGB (`R16G16B16A16_FLOAT` + G10_P709) and HDR10 (`R10G10B10A2` + G2084_P2020) color spaces, including `SetHDRMetaData`?
5. **winit #4233 in practice:** how often does a lost key-up actually occur for our key set, and is the raw-input/`GetAsyncKeyState` net sufficient, or do we need the Windows Raw Input backend ourselves?
6. **VRR / G-Sync** on this panel: how does it interact with `Mailbox`/allow-tearing and our "one advance per refresh" cap? Should the cap use max refresh as the bound?
7. **winit master/0.31 migration cost:** the `MonitorHandle` -> `MonitorHandleProvider` + `current_video_mode()` refactor and `user_event` -> `user_wake_up` — when to adopt.
8. **Presenting off-thread:** do we present from the input thread or a render thread? (wgpu v28.0.1 fixed an NVIDIA off-thread present crash — confirm our wgpu version includes it.)

---

## Sources

**winit / input / refresh rate**
- KeyEvent (repeat field): https://docs.rs/winit/latest/winit/event/struct.KeyEvent.html
- winit issue #4043 — Windows key-repeat input lag: https://github.com/rust-windowing/winit/issues/4043
- winit issue #4233 — use Raw Input for physical key events (lost key-up): https://github.com/rust-windowing/winit/issues/4233
- winit Meta issue #1806 — keyboard input: https://github.com/rust-windowing/winit/issues/1806
- MonitorHandle (0.30.12, stable API): https://docs.rs/winit/0.30.12/winit/monitor/struct.MonitorHandle.html
- MonitorHandle (master, future API): https://rust-windowing.github.io/winit/winit/monitor/struct.MonitorHandle.html
- VideoMode: https://docs.rs/winit/0.20.0-alpha4/winit/monitor/struct.VideoMode.html
- Fullscreen enum: https://docs.rs/winit/latest/winit/window/enum.Fullscreen.html
- ApplicationHandler: https://docs.rs/winit/0.30.5/winit/application/trait.ApplicationHandler.html
- winit 0.30 changelog: https://rust-windowing.github.io/winit/winit/changelog/v0_30/index.html
- sdl3-rs: https://github.com/vhspace/sdl3-rs ; docs: https://docs.rs/sdl3/latest/sdl3/

**wgpu**
- PresentMode: https://docs.rs/wgpu/latest/wgpu/enum.PresentMode.html
- Queue (write_texture/write_buffer): https://docs.rs/wgpu/latest/wgpu/struct.Queue.html
- StagingBelt: https://docs.rs/wgpu/latest/wgpu/util/struct.StagingBelt.html
- StagingBelt textures issue #1444: https://github.com/gfx-rs/wgpu/issues/1444
- write_texture performance discussion #5899: https://github.com/gfx-rs/wgpu/discussions/5899
- CHANGELOG (HDR color space, Fifo pacing, DX12 waitable, off-thread present; latest v29.0.1): https://github.com/gfx-rs/wgpu/blob/trunk/CHANGELOG.md
- PR #7400 — disable waiting for latency waitable object (Dx12BackendOptions, v27): https://github.com/gfx-rs/wgpu/pull/7400
- issue #8354 — DXGI swapchain interop for Vulkan backend: https://github.com/gfx-rs/wgpu/issues/8354
- issue #2920 — well-defined HDR surface support: https://github.com/gfx-rs/wgpu/issues/2920
- learn-wgpu HDR tutorial: https://sotrh.github.io/learn-wgpu/intermediate/tutorial13-hdr/

**DXGI / D3D12 / low-latency presentation**
- DXGI flip model (DirectX devblog): https://devblogs.microsoft.com/directx/dxgi-flip-model/
- For best performance, use DXGI flip model (Win32 docs): https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/for-best-performance--use-dxgi-flip-model
- Reduce latency with DXGI 1.3 swap chains (waitable object usage): https://learn.microsoft.com/en-us/windows/uwp/gaming/reduce-latency-with-dxgi-1-3-swap-chains
- GetFrameLatencyWaitableObject: https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject
- Variable refresh rate displays (ALLOW_TEARING, sync interval 0): https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/variable-refresh-rate-displays
- NVIDIA Advanced API Performance: Swap Chains: https://developer.nvidia.com/blog/advanced-api-performance-swap-chains/
- Raph Levien — Swapchains and frame pacing: https://raphlinus.github.io/ui/graphics/gpu/2021/10/22/swapchain-frame-pacing.html
- Present Latency, DWM and Waitable Swapchains (jackminnet): https://jackmin.home.blog/2018/12/14/swapchains-present-and-present-latency/

**HDR / wide gamut**
- MS DirectX Advanced Color / HDR (scRGB, HDR10 formats & color spaces): https://learn.microsoft.com/en-us/windows/win32/direct3darticles/high-dynamic-range
- WebGPU-native WGPUSurfaceColorManagement: https://webgpu-native.github.io/webgpu-headers/structWGPUSurfaceColorManagement.html
- What's New in WebGPU (Chrome 129) — rgba16float + extended tone mapping: https://developer.chrome.com/blog/new-in-webgpu-129

**macOS / Metal**
- Daniel Hooper — lowest-latency macOS CAMetalLayer settings: https://x.com/DanielcHooper/status/2067320915545686158
- CAMetalLayer.displaySyncEnabled: https://developer.apple.com/documentation/quartzcore/cametallayer/displaysyncenabled
- CAMetalLayer: https://developer.apple.com/documentation/quartzcore/cametallayer
