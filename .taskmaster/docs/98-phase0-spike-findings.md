# Task #98 — Phase 0 enumeration spike findings (2026-07-14)

Throwaway spikes (`pb-decode/examples/spike_tracks.rs`, `spike_avtracks.rs`, both deleted
after this record) run against purpose-built fixtures, to de-risk the catalog model before
committing it. **Every load-bearing API the plan assumed is confirmed**, with four
corrections that changed the design.

## FFmpeg — CONFIRMED, with one correction

Ran over `multitrack.mkv` (2 audio / 4 subtitle) + `tone_51.mp4` + `color_with_tone.mp4`.

- `avcodec_get_name((*par).id)` → `"aac"`, `"ac3"`, `"subrip"`, `"hdmv_pgs_subtitle"`. ✅
- `stream.metadata()` `language` **and** `title` read, incl. non-Latin (`"日本語字幕"`). ✅
- `Disposition::{DEFAULT,FORCED,COMMENT,HEARING_IMPAIRED,VISUAL_IMPAIRED}` all read
  correctly off the safe bitflags — no ffi walk needed. ✅
- `av_channel_layout_describe(&ch_layout, buf, len) -> c_int` via the bounded-FFI-buffer
  pattern → `"stereo"`, `"5.1(side)"` (MKV AC-3), `"5.1"` (MP4 AAC), `"mono"`. Matches the
  `ffprobe` oracle exactly. Return code is *bytes needed*: `< 0` = AVERROR, `> buf_size`
  = truncated. ✅
- `(*par).ch_layout.order` is the honest **"is the layout actually known"** signal:
  `AV_CHANNEL_ORDER_NATIVE` = real named layout; `AV_CHANNEL_ORDER_UNSPEC` = unknown, and
  describe() then prints `"N channels"`. So the plan's "never invent 5.1" rule is
  implemented by gating on `order`, not by string-sniffing describe()'s output. ✅

**CORRECTION — profile constants collide across codecs.** `(*par).profile` is only
meaningful *per codec id*:

    AV_PROFILE_DTS_ES == AV_PROFILE_EAC3_DDP_ATMOS == AV_PROFILE_TRUEHD_ATMOS == 30

so a codec-blind profile→name map would render an Atmos TrueHD track as "DTS-ES". The
profile map **must** be keyed on the codec first. Also `AV_PROFILE_UNKNOWN == -99` (AC-3
reports it), which must not fall through to a DTS name.

## AVFoundation — CONFIRMED, with three corrections

Ran over `multitrack.mp4` (2 AAC audio, 2 tx3g subtitle) and `multitrack.mkv`.

- Audible **and** legible `AVMediaSelectionGroup`s exist on an ffmpeg-authored MP4 (the
  open question) — audible: 2 options + a `defaultOption`; legible: 4 options. ✅
- **`propertyList` identity round-trips**: `option.propertyList` →
  `group.mediaSelectionOptionWithPropertyList:` → `isEqual:` the original ⇒ **true for
  every option in both groups**. This is the identity `TrackLocator::AvOption` needs. ✅
- `propertyList` serializes to a binary plist via `NSPropertyListSerialization`
  (208–331 bytes/option) ⇒ `property_list: Vec<u8>` is viable. ✅
- `extendedLanguageTag` / `displayName` / `mediaSubTypes` / `mediaCharacteristics` all
  read via `objc_msgSend`. ✅
- MKV/WebM → **nil groups** (AVFoundation can't demux) ⇒ the `--ffvideo` FFmpeg fallback
  owns those, exactly as the plan says. ✅

**CORRECTION 1 — a wrong selector is an uncatchable crash, not an error.** The plan's
`playable` is the *property* name; the ObjC getter — and therefore the selector — is
`isPlayable`. Sending `playable` aborts the process with an uncaught
`NSInvalidArgumentException` that `catch_unwind` cannot contain. Production code guards
every non-obvious selector with `respondsToSelector:`.

**CORRECTION 2 — `extendedLanguageTag` is BCP-47 short, not ISO-639-2.** AVFoundation
returns `"en"` / `"fr"` where FFmpeg returns `"eng"` / `"fra"` for the same content. The
language display map must accept **both** 2- and 3-letter tags (and pass through the rest),
which settles the plan's open "isolang vs hand lookup" question in favour of a map keyed on
both forms. `displayName` is a *localized* name ("English", "English Forced") — **not** the
container title, so it must not be used as `MediaTrack::title`.

**CORRECTION 3 — AVFoundation synthesizes options; the enumeration is genuinely not 1:1
with streams.** 2 authored tx3g subtitle tracks became **4** legible options (English,
English Forced, French, French Forced). Its `defaultOption` for the legible group was
"English Forced" — *not* the track authored `default`. This is the strongest vindication of
the plan's `backend: MediaBackend` field and of "keep authored `defaultOption` separate from
AVPlayer's runtime auto-selection": the same file honestly enumerates differently per
backend, and the catalog must say which backend produced it rather than pretend to a single
truth. It also means **MP4 archive parity is per-backend**, not byte-identical (an MKV is
FFmpeg on both paths, so it *is* identical).

Also: `mediaCharacteristics` string values are a mix — some are `public.*`
(`"public.main-program-content"`, `"public.subtitles.forced-only"`), others are literal
(`"AVMediaCharacteristicVisual"`, `"AVMediaCharacteristicFrameBased"`). Compare against the
framework constants, never hardcoded literals.

## Media Foundation — NOT RUN (no Windows host)

This session is macOS-only, so the `GetNativeMediaType` loop / `MF_E_INVALIDSTREAMNUMBER`
termination / language-and-title location questions are **unanswered**, and the `cfg(windows)`
code cannot even be compile-checked here. Per the plan's verification item 4, MF is therefore
implemented conservatively and **recorded as pending a Windows run** rather than claimed.
