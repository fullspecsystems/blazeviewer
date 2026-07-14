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

## Media Foundation — RUN 2026-07-14 (Windows box, subtask 98.5)

Spike `pb-decode/examples/spike_mftracks.rs` (throwaway, deleted after this record) dumped
**every attribute by index** on each native `IMFMediaType` *and* each `IMFStreamDescriptor`
over `multitrack.mkv`, `multitrack.mp4`, `tone_51.mp4`, `color_with_tone.mp4`,
`black_then_color.mp4`, `tone_vp9_opus.webm`. All three open questions answered, plus one
trap the plan did not anticipate. Backend: `pb-decode/src/mf_tracks.rs`.

**Q1 — the loop terminator: CONFIRMED.** `GetNativeMediaType(i, 0)` returns
`0xC00D36B3` = `MF_E_INVALIDSTREAMNUMBER` at the first index past the end, on every
container tested (MP4, MKV, WebM). The loop terminates. Pinned by
`the_native_media_type_walk_ends_on_invalid_stream_number`, so a future Windows changing the
code fails a test rather than silently truncating (or never ending) the walk.

**Q2 — language/title location: the plan's warning was RIGHT.** They are **not** on
`IMFMediaType` (the by-index dump shows neither, on any stream of any fixture). They live on
the **stream descriptor**: `MF_SD_LANGUAGE` and `MF_SD_STREAM_NAME`, reached via
`GetServiceForStream(MF_SOURCE_READER_MEDIASOURCE)` → `CreatePresentationDescriptor` →
`GetStreamDescriptorByIndex`. Two follow-ons:
- **MF reports BCP-47 short tags** — `"en"` / `"fr"` where FFmpeg says `"eng"` / `"fra"` for
  the same file. Exactly AVFoundation's *correction 2*; `language_display` already resolves
  both, so no map change was needed. (Third independent confirmation that keying the map on
  both forms was right.)
- `MF_SD_STREAM_NAME` = `"Director's Commentary"` on **both** the MKV and the MP4 — for the
  MP4 it is the `hdlr` handler name, which `ffprobe` does *not* surface as `tag:title`. With
  no dispositions available (Q4), this title is the **only** surviving commentary signal, and
  the formatter's title-derived "Commentary" rule carries it through.
- A PD created *after* `open_video_reader`'s `SetStreamSelection(ALL, false)` still reports
  the authored attributes unchanged (measured) — the reader's selection does not poison it.

**Q3 — subtitles: they do not enumerate at all.** `multitrack.mkv`'s **4** subtitle tracks
(SubRip ×3 + PGS) and `multitrack.mp4`'s **2** tx3g tracks both come back as **nothing** —
stream-descriptor count 3 in both cases (video + 2 audio), no `MFMediaType_Subtitle` anywhere.
MF defines `MFSubtitleFormat_*` GUIDs but the MKV/MP4 sources never expose a subtitle stream.
So the backend reports `subtitles: Unavailable` — **never** `complete(vec![])`, which would
render "Subtitles: No" about a file with four of them. And a *non-empty* subtitle set (no
fixture produces one) is reported `Partial`, not `Complete`: having proven MF drops subtitle
tracks that exist, we cannot claim it ever showed us all of them.

**CORRECTION — `IMFPresentationDescriptor`'s `selected` flag is NOT the authored default.**
The trap. It looks exactly like a default flag and is wrong:

| | `ffprobe` `disposition:default` | MF `selected` |
|---|---|---|
| `multitrack.mp4` audio `eng` | **1** | **false** |
| `multitrack.mp4` audio `fra` (commentary) | 0 | **true** |

MF simply takes the **first stream of the `MF_SD_MUTUALLY_EXCLUSIVE` group**. Mapping
`selected` → `TrackFlags::default` would have labelled the **Director's Commentary as
"Default"**. (It *coincidentally* matches on the MKV, which is how this ships unnoticed.) MF
exposes no authored disposition at all — no default, forced, commentary, SDH or AD — so the
backend claims **`TrackFlags::none()` for every track** rather than a plausible guess. Pinned
by `no_dispositions_are_claimed_because_mf_exposes_none`.

**Also — MF reorders streams relative to the container.** `ffprobe` has the MP4's English
audio at index 1 and French at 2; **MF's ordinal 0 is the French one** (cross-checked via
`MF_MT_AVG_BITRATE` 31480 ≈ ffprobe's 31484, vs 30672 ≈ 30679). Two consequences: the
stream-descriptor index **does** correspond to the source-reader ordinal (confirmed
empirically by that same bitrate pairing, not just by the docs), and an FFmpeg stream index
would resolve to the **wrong track** — vindicating `TrackLocator::MfStream` as its own
namespace. The backend still guards the pairing by comparing the descriptor's handler major
type to the media type's, dropping the metadata rather than stapling one track's language
onto another.

**Honest degradations (recorded, not bugs):**
- **No named channel layout.** No fixture's native audio type carries
  `MF_MT_AUDIO_CHANNEL_MASK` — not the 5.1 AC-3, not the 5.1 AAC. So `layout: None` and the
  formatter prints "6 channels" where FFmpeg prints "5.1". This is the "never invent 5.1"
  rule working as designed.
- **DTS variants collapse to "DTS".** The marketing names ("DTS-HD MA") are derived from
  FFmpeg's *profile* int, which MF has no equivalent of; synthesizing one from
  `MFAudioFormat_DTS_XLL` would be inventing a fact. `profile: None` is passed always.

Net: **audio is `Complete`** (MF's audio stream table matched `ffprobe` exactly on every
fixture — 2/2 on both multitrack containers, 0 on the silent clip, 1 elsewhere, so the
`Complete` + `total: Some(0)` that renders "Audio: No" is earned), **subtitles are
`Unavailable`**, and **dispositions are absent**. MF genuinely sees less than FFmpeg; the
catalog says so through completeness + `backend` rather than pretending to a single truth —
which is the third independent vindication of that design choice.
