# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 10). Supersedes rev 9. **Audio track selection is built for
Windows + Linux** on branch `feat/audio-track-selection` (2 commits, pushed) — the FFmpeg→MF
bridge rev 9 called for, plus the whole switch path._

## What we worked on today

- **The FFmpeg→MF audio bridge** (`89b8cc9`): `pb_decode::tracks::bridge_mf_audio_locators`
  matches FFmpeg audio rows to MF twins by (language-resolved, codec family, channels) →
  (codec, channels) for language-less MF tracks → order within identical groups, and stamps
  `MfStream` locators onto the runtime catalog in `media_details.rs` before FFmpeg supersedes
  MF. `MfAudioDecoder::open_reader_stream` + `reader_stream()`; core accessors
  `audio_row_mf_stream` / `audio_row_for_{mf,ff}_stream`.
- **The switch** (`07ed43f`): `WasapiAudio` `Cmd::SetTrack` (new decoder opens+seeks before
  playback is touched — a refusal keeps the sound; dead engine answers `false`), Linux pw-cat
  feeder `SetTrack` (new pipeline built before old retired), `SelectAudioTrack` wired in
  `main.rs`, Playback ▸ Audio Track flyout (sig-guarded rebuild; Linux egui bar gets
  "Next Audio Track"), tick pinned to the engine's `active_track`. Also fixed the broken
  Linux pb-app build (`MenuState` not `Copy`).
- Verified: fixture tests (tone-proven stream selection, bridged locators across MF's
  reordering), corpus probe (Apollo 13: AC-3 main + 2 identical AAC commentaries bridge to
  MF 1/2/3), clippy clean on default + `libheif,dav1d,ffprobe`, WSL stub-feature check.

## Remaining / constraints

- **No audio endpoint over RDP** — the engine e2e test skipped; owner must confirm sound +
  menu on the desktop (`PB_SUBTITLE_TRACE=1` prints the audio flyout; Apollo 13 is the test
  film: commentaries switch, the AC-3 main refuses honestly until the AC-3 decode task).
- **Linux `ffvideo` half is compile-unverified** (WSL Ubuntu 24.04 lacks FFmpeg 8) — owner
  tests on a real VM.
- AC-3/E-AC-3 still undecodable on Windows (pre-existing, worth its own task).

## Next

1. Owner desktop verification → merge branch to main.
2. AC-3/E-AC-3 decode task (makes DD/DD+ mains audible *and* re-switchable).
3. Linux VM run of the ffvideo audio path.
