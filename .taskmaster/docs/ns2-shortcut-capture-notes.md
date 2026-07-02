# NS2.6 shortcut-capture editor — ideas borrowed from sindresorhus/KeyboardShortcuts

Decision (2026-07-02, owner): the keybinding editor is **bespoke** — the
KeyboardShortcuts package is disqualified because its Recorder rejects exactly the
chords PhotoBlaze lives on (`RecorderCocoa.swift`: a shortcut must have a non-shift
modifier or be a function key; bare Space/arrows/letters and Shift-only chords beep
and are refused — that validation is correct for its design center, global Carbon
hotkeys, and wrong for our app-local keymap). But the package is **MIT**, small
(~4.6k lines), and full of hard-won details. We reviewed the source and lift the
ideas below. Repo: <https://github.com/sindresorhus/KeyboardShortcuts> (reviewed at
v3.0.1; re-clone for reference — file/line refs below are from that tag).

## Lift nearly verbatim (MIT, attribute in a comment)

- **`LocalEventMonitor`** (`Utilities.swift` ~L69): a ~30-line RAII wrapper over
  `NSEvent.addLocalMonitorForEvents(matching:handler:)` — start/stop/deinit. This
  IS the capture mechanism the NS2 plan calls for. Key property we rely on: a local
  monitor sees events **before** `sendEvent`/menu dispatch, so returning `nil`
  swallows the event — a captured `⌘Q` must not quit the app mid-recording.
- **`NSAlert` sheet helpers** (`Utilities.swift` ~L214): `NSAlert.showModal(for:
  window)` = `beginSheetModal` + `NSApp.runModal(for:)` → a *synchronous*
  window-attached sheet. Directly reusable for NS2 item 2 (Confirm/Message).
- **Modifier symbol rendering** (`Utilities.swift` `ks_symbolicRepresentation`):
  canonical macOS glyph order is **⌃ ⌥ ⇧ ⌘** (control, option, shift, command).
- **Special-key glyph table** (`Shortcut.swift` `presentableDescription`): ↩ ⌫ ⌦ ⎋
  ⇥ ⇞ ⇟ ↑ → ↓ ←, "Space" spelled out (matches macOS), F1–F20 as text, keypad keys
  via U+20E3 combining-keycap (e.g. `7⃣`).

## The recording-state event protocol (their choreography, minus the modifier guard)

While a row is recording, the local monitor watches `.keyDown` + `.leftMouseUp` +
`.rightMouseUp` and:

1. **Click outside the control** (bounds inset by −3 px margin) → end recording,
   and `return event` so the click still lands where the user aimed.
2. **Bare Tab** → end recording and `return event` (bubbles up → focus moves on).
3. **Bare Esc** → cancel recording, swallow.
4. **Bare Delete/Backspace/⌦** → clear the binding, swallow.
5. Anything else → capture as the chord, save, end recording, swallow.

⚠ **PhotoBlaze delta:** rules 2–4 reserve keys our keymap actually binds
(Backspace = prev photo, Esc = quit, Enter = random). **Resolved against the egui
editor (2026-07-02, owner-confirmed):** `dialog.rs::handle_capture_event` reserves
*only* Esc (cancels capture, binding unchanged) — Backspace/Tab/Enter/etc. ARE
capturable (any non-modifier key binds the armed slot, stealing the chord from a
prior owner, with a "Moved X from Y" note). So the bespoke editor should mirror
that: Esc = cancel, everything else capturable, clear via a per-row button (the
egui editor uses a ✕ button per slot, not ⌫-to-clear). **Esc being unbindable
through the UI is a deliberate punt** ("worry about it when it becomes a
problem"). Note the platform asymmetry if it ever does: the winit shell
*pre-filters* Esc ahead of the keymap entirely (`main.rs` KeyboardInput arm:
dialog-dismiss / picker esc-guard / `begin_exit` — Esc never reaches
`handle(KeyDown)`), so a hand-edited TOML `Escape` binding is inert there; the
Swift host forwards Esc through the keymap (`key_down("Escape")` → Quit), so it
*would* take effect on Mac. If rebinding Esc ever matters, the fix is a two-stage
commit (pending chord + ✓/✕ buttons, mouse-only cancel) — don't invent it before
someone asks.

## Correctness details worth copying

- **Modifier normalization** (`Utilities.swift` `NSEvent.modifiers`): intersect
  with `.deviceIndependentFlagsMask`, then subtract `.capsLock` (must not affect a
  chord) and `.numericPad` (**arrow keys spuriously set it**). They also subtract
  `.function` when building a shortcut from an event (`Shortcut.init(event:)`) —
  Fn+F1 vs Fn+V display can't be reliably distinguished, so Fn is not modeled.
- **Layout-aware display** (`Shortcut.swift` `keyToCharacter()`): translate the
  physical key code through the *current keyboard layout* for display —
  `TISCopyCurrentASCIICapableKeyboardLayoutInputSource` →
  `UCKeyTranslate(kUCKeyActionDisplay, no modifiers)`. An AZERTY user sees the
  letter their key actually types, not the ANSI position. **Main thread only**
  (`TISGetInputSourceProperty` crashes off-main). Our capture stores the PbKey
  (physical, via the existing `KeyMap.swift` Carbon table); this is display-only.
- **Focus lifecycle** (`RecorderCocoa.swift` `viewDidMoveToWindow`): end recording
  on `windowDidResignKey` **and** drop first responder — since macOS 13.5 a
  Settings window *hides* instead of closes on ✕, so a recording can otherwise
  survive a "closed" window. Also their `preventBecomingKey` dance: the recorder
  must refuse initial focus when the settings window opens/unhides, or it starts
  recording the moment the window appears.
- **Suspend normal key handling while recording** (their `isPaused` +
  `recorderActiveStatusDidChange` notification): our analog is gating the canvas/
  `KeyMap.swift` forwarding to the Rust core while a capture row is active, so the
  swallowed events also never become `key_down` FFI calls.

## Conflict detection (build a lighter version)

- **System shortcuts:** Carbon `CopySymbolicHotKeys()` → array of dicts; keep
  entries where `kHISymbolicHotKeyEnabled`, read code+modifiers (`HotKey.swift`
  ~L533). They exempt bare F12 (historically listed but unused). Their default
  policy: **warn with "Use Anyway"**, don't block.
- **Own-menu conflicts:** recursive walk of `NSApp.mainMenu` comparing
  `keyEquivalent` + `keyEquivalentModifierMask` (`Shortcut.swift`
  `menuItemsWithMatchingShortcut`). Subtlety: an *uppercase* `keyEquivalent`
  implies ⇧ — normalize by lowercasing + inserting `.shift` before comparing.
  Track the pre-recording chord so re-capturing the same value doesn't warn
  against its own menu item. Their default: **block**. For us the same check
  should also run against the **rest of our own keymap** (other actions) — their
  `validateShortcut` closure slot is the shape for it.
- Their `ConflictPolicy` block/warn/allow enum is more machinery than we need;
  hardcode: own-keymap + own-menu = warn-and-offer-reassign, system = warn.

## Explicitly NOT borrowed

UserDefaults storage (ours is the core-owned TOML `Keymap` via
`SettingsSaved{keymap}` / `KeymapSubmitted`), Carbon `RegisterEventHotKey` global
registration (app-local only), the no-bare-keys validation (the disqualifier), the
sandboxed-macOS-15.0/15.1 "disallowed shortcut" workaround (we're not sandboxed),
and the SwiftUI `Recorder` wrapper (our rows are bespoke). Their NSSearchField
base (free ✕ clear button) is cute but ties the control to a search-field look;
our row layout comes from the Settings form design instead.
