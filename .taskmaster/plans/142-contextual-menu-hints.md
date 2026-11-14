# #142 — Menu shortcut hints follow context (Next / Play/Pause over a live video)

Owner report 2026-11-13: while a video plays, Space pauses and Shift+Space goes to the
next item (task #94), but the menu still advertised Space on Next and P on Play/Pause.

## Design

- **One rule, in the core:** `Keymap::menu_hint(action, space_toggles_video)` returns the
  key a menu item shows right now. Normally that's the action's primary binding. While a
  live video owns the Next keys (`AppCore::space_toggles_video`), Next shows Skip Next's
  key (⇧Space) and Play/Pause shows Next's (Space), falling back to its own (P) if Next is
  unbound. It reads the user's bindings, so rebinds carry through.
- **Context rides `MenuState`.** The flag was `skip_next_visible` (named for its first
  consumer, the Mac's Playback ▸ Skip to Next Item). It's renamed `space_toggles_video` for
  the condition it describes, now that the hints read it too. Both shells already set it
  from `space_toggles_video()` and diff `MenuState` per tick, so a flip re-applies the menu.
  Windows/Linux set it now as well (they didn't before).
- **Shells only format.** macOS: `menu_hint_display` FFI → `MenuBar.refreshShortcutBadges`,
  re-run from `sync` when the flag flips. Windows: `menu::ContextHints` holds the Next and
  Play/Pause items and re-labels them in `apply_menu_to_native`. Linux: the egui bar's
  shortcut column calls `labeled_in` with the flag.
- **Drift guard:** `menu_hints_do_what_they_say_over_a_live_video` presses the hinted
  chords through the real keymap lookup + `nav_press` and checks Play/Pause's key pauses
  and Next's navigates. Mutation-checked: pointing Next's hint back at Next fails it.

**Is this general enough?** Yes, for now. `nav_press`'s Space override is the only context
rule that changes what a *menu-listed* action's key does (arrow-seek has no menu items).
A future one adds a match arm in `menu_hint` (plus a context flag on `MenuState` if it's a
new condition) and a line in the parity test. A table shared by the input path and the
hints would only pay off with several such rules, and the arrow-seek override depends on
pan overflow, which a table can't express cleanly.

## Handoff

**Verified (macOS, Buzz, 2026-11-13):**
- `cargo clippy --all-targets` on the mac-swift set with `-D warnings`: clean.
- `cargo test -p pb-app-core -p pb-mac-ffi`: 1027 + 39 passed, 4 ignored (+4 on 1023: three
  `menu_hint` unit tests and the parity test).
- The parity test fails when `menu_hint` is broken on purpose (Next's hint pointed back at
  Next), so it guards what it claims to.
- `build-swift-host.sh --release --bundle-ffmpeg` builds and the FFmpeg audit is OK.
- Windows compile check from the Mac: `cargo clippy -p pb-app --all-targets --target
  x86_64-pc-windows-msvc -- -D warnings` clean (bin + its tests; the usual temporary
  blake3/ureq Cargo edits, reverted). It caught one real issue first: the old
  `shortcut_hint` had become test-only, a dead-code error on Windows. It's now the single
  context-aware function. (Buzz needed `rustup target add x86_64-pc-windows-msvc` first.)

**Not verified:**
- **Windows run:** the `pb-app` changes (`menu.rs` `ContextHints`, `main.rs` field +
  `apply_menu_to_native` + `current_menu_state`) compile but have never run, and the
  `menu.rs` unit tests (incl. the new `labeled_in` asserts) only run on Windows. Run
  `cargo test -p pb-app`, then play a video and check Image ▸ Next reads "Shift+Space" and
  Playback ▸ Play/Pause reads "Space", and that both return to "Space" / "P" when the clip
  ends or you move to a photo.
- **Linux:** the egui bar change (`menu_bar_spec`'s `l` closure) is `cfg(linux)` and not
  compiled anywhere yet. It's two lines; next Linux build (AppImage) will catch a typo.
- **macOS by hand:** the same check as Windows, in the Mac menu (badges).

**Cross-platform debt:** none beyond *Not verified* — every shell's change is in this commit.

**Decisions / corrections:**
- 2026-11-13, owner: Play/Pause shows Space while a video plays (P still works), since
  Space is the key people reach for then.

**Claimed:** the Mac session (Buzz) wrote all three shells' changes in one commit. Windows
only needs to verify, not write.
