# Packaging & Distribution (Windows)

PhotoBlaze ships as a **code-signed WiX/MSI** installer, built and signed in
GitHub Actions and attached to Releases. This is the decision recorded in
[`decisions.md`](./decisions.md) **ADR-018** (a signed classic installer over
MSIX/Store — same SmartScreen-free result, far cheaper folder-verb registration,
no container friction; MSIX/Store is revisited only if/when monetizing).

## What the installer does

- Installs `photoblaze.exe` to `C:\Program Files\PhotoBlaze\bin\`.
- Registers a **ProgId** (`PhotoBlaze.Image`) whose open command is
  `"…\photoblaze.exe" "%1"`, with the app's embedded icon.
- Lists `PhotoBlaze.Image` under **`OpenWithProgids`** for the common raster
  types — `jpg jpeg jpe jfif png gif bmp tif tiff webp heic heif avif jxl` — so
  PhotoBlaze appears in **Open with** *without* seizing any default. (RAW/SVG are
  intentionally not claimed.)
- Adds an **"Open with PhotoBlaze"** verb to the folder right-click menu (and the
  folder background), launching `"…\photoblaze.exe" "%V"`. Opening a folder
  browses it **recursively** by default (toggle at runtime with `Ctrl+R`).
- Adds a Start-menu shortcut and a clean uninstaller.

### The default-viewer caveat

No app or installer can silently become the default image handler on Windows
10/11 — the choice is stored in a SID-salted `UserChoice` hash the OS protects.
The MSI only registers PhotoBlaze as a *candidate*; the user confirms via
*Open with → Choose another app → Always*, or Settings. A future in-app "Set as
default" button (Task 14) just deep-links `ms-settings:defaultapps`.

## Prerequisites (local build)

- **Rust** (stable; `rust-toolchain.toml`).
- **WiX Toolset v3** (candle/light). GitHub's `windows-latest` runners ship it
  preinstalled (the `WIX` env var). Locally, install WiX 3.11/3.14 and ensure
  `candle.exe`/`light.exe` are on `PATH` or `WIX` is set.
- **cargo-wix**: `cargo install cargo-wix --locked`.
- A **resource compiler** (`rc.exe` from the Windows SDK, or `llvm-rc`) for the
  embedded .exe icon. This is *best-effort* — if absent, `build.rs` warns and the
  build still succeeds (the runtime window icon and the MSI's own icon still work).

## Build the MSI locally

```sh
cargo wix --package pb-app --nocapture
# -> target/wix/PhotoBlaze-<version>-x86_64.msi
```

`cargo-wix` reads the version from `Cargo.toml` and compiles
[`crates/pb-app/wix/main.wxs`](../../crates/pb-app/wix/main.wxs). To package an
already-built (e.g. already-signed) binary without rebuilding:

```sh
cargo build --release -p pb-app
cargo wix --package pb-app --no-build --nocapture
```

> A benign `ICE69` warning ("shortcut Target references a file in another
> component") is expected — it's the standard Start-menu-shortcut pattern.

## Signing (Azure Trusted Signing)

Signing removes the SmartScreen prompt; it is **not** tied to the packaging
format. We use Azure Trusted Signing via `signtool`/the
`azure/trusted-signing-action`. The exe is signed *before* packaging (so the
installed binary is trusted) and the MSI is signed after.

The certificate's **subject must match** the manufacturer identity. Required CI
configuration (see [`../../.github/workflows/release.yml`](../../.github/workflows/release.yml)):

| Kind   | Name | Example / meaning |
|--------|------|-------------------|
| secret | `AZURE_TENANT_ID` | service-principal tenant |
| secret | `AZURE_CLIENT_ID` | service-principal app id |
| secret | `AZURE_CLIENT_SECRET` | service-principal secret |
| var    | `AZURE_SIGNING_ENDPOINT` | e.g. `https://eus.codesigning.azure.net/` |
| var    | `AZURE_SIGNING_ACCOUNT` | Trusted Signing account name |
| var    | `AZURE_SIGNING_PROFILE` | certificate profile name |

The service principal needs the **Trusted Signing Certificate Profile Signer**
role on the account. If the secrets are absent the workflow still builds an
**unsigned** MSI (with a warning) for testing.

## Release flow

`release.yml` triggers on a `v*` tag:

1. `cargo build --release -p pb-app` (embeds the icon via `build.rs`).
2. Sign `target/release/photoblaze.exe`.
3. `cargo wix --no-build` → `target/wix/*.msi`.
4. Sign the MSI.
5. Upload as a build artifact and attach to the GitHub Release.

```sh
git tag v0.1.0 && git push origin v0.1.0   # cuts a signed release
```

## The icon pipeline

- Source art: `crates/pb-app/icons/photoblaze.png` (1024²).
- `crates/pb-app/icons/photoblaze.ico` is a committed multi-size icon
  (16–256 px) generated from the PNG. Regenerate it with any PNG→ICO tool if the
  art changes (the one used was a tiny `image` + `ico` Rust helper).
- `build.rs` embeds the `.ico` + version metadata into the `.exe` (Explorer /
  taskbar / association glyph).
- At runtime the window/taskbar icon is decoded from the embedded PNG
  (`load_window_icon`), so it applies even on a build without a resource compiler.
- The MSI references the same `.ico` for Add/Remove Programs and the shortcut.

## macOS (future — Task 15)

The same `pb-core::open` launch seam is reused; only delivery is new: a `.app`
bundle with `Info.plist` UTIs, the `openFiles` Apple Event, Developer-ID signing,
notarization, and a `.dmg`. No renderer or playlist changes (wgpu already targets
Metal — ADR-002a).
