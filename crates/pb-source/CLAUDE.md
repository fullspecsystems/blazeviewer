# pb-source — archive internals (crate-local context)

Auto-loads when working in `crates/pb-source/`. Root `CLAUDE.md` has the
summary; the "doors" (deck-integration) side lives in
`crates/pb-app-core/CLAUDE.md`. Maintained here since 2026-07-19.

## Archive viewing (ZIP + 7z + the tar family + RAR5 + RAR4) — tasks #30, #102, #103

Wired behind the `ItemSource` seam, decoded via `pb_decode::decode_named_bytes`
(bytes + extension hint). One classifier — `pb_source::archive_kind` — answers
every "is this an archive, and which kind?" question (shell `is_archive`
predicates, `scan::open_archive` dispatch, the `LibraryItemKind` door arm, double
extensions like `.tar.gz`, `.cbr`/`.cbz` comics).

- **Two access models:** **lazy** (ZIP via handle pool; plain `.tar` via a
  seek-over-data header index) and **eager decode-to-RAM** (7z; `.tar.gz` /
  `.tar.bz2` / `.tar.zst` / `.tar.xz` — solid streams have no cheap random
  access). 7z pre-flights its RAM budget from the header; a compressed tar has no
  size table, so its budget is enforced **mid-stream** (`OpenError::TooLarge`,
  still refuse-before-reserve).
- **Off-thread opens:** every kind but ZIP opens off-thread
  (`ArchiveKind::background_open`) through the one worker entry
  `scan::load_archive`, with determinate progress + Cancel.
- **Hostile-bytes hardening (tar):** metered PAX/GNU metadata quota,
  entry/name-table caps, expanded-work cap, zstd window pre-check +
  frame-checksum verify, xz dict-size pre-check; `fuzz/` has `tar_open` +
  `rar_open` targets — see the #102 plan rev2.
- **RAR5 + RAR4 (#103):** two of our own container parsers (`rar.rs` = RAR5,
  `rar4.rs` = RAR4 — completely different container shapes) over the `compcol`
  codecs (`rar5` + `rar3`; exact-pinned fork rev on the `rar3-standard-filters`
  branch until it merges + releases on crates.io). Both share one
  `RarSource`/`ItemSource` via an `EntryData::Lazy { codec: RarCodec::{Rar5,Rar3} }`
  tag: non-solid lazy / solid eager, header-CRC + entry-CRC32 verified,
  corpus-validated byte-identical to `unrar` (44 archives / 218 entries in the
  differential). RAR5 Delta/x86 filters decode (compcol's `add_file_boundary`
  makes position-dependent filters file-relative in a solid group); RAR4 LZ/PPMd
  + Delta/x86/audio filters + solid all decode. Multi-volume, encrypted RAR4
  headers (`-hp`), and unsupported encryption versions refuse with honest
  messages (`ArchiveOpenError::Unsupported`); a codec-refused member degrades
  per-entry (its solid-group tail goes unavailable), the rest of the archive
  serves.
- **RAR5 encryption** (`rar_crypt.rs`): per-file (`-p`) and full-header (`-hp`)
  RAR5 use standard PBKDF2-HMAC-SHA256 + AES-256-CBC (the tractable scheme,
  unlike RAR4's bespoke SHA-1 KDF — RAR4 `-p` refuses per-entry, `-hp` refuses at
  open), so a missing/wrong password returns `PasswordRequired` (prompts, like
  ZIP/7z) and a correct one decrypts — validated byte-identical to `unrar` over
  the corpus and a committed encrypted-solid fixture. ⚠️ RAR5 encrypted solid
  runs are padded to 16 bytes *between* files, so each run is decrypted then
  stripped to its real block length (`rar5_stream_len`) before the LZ decoder,
  which reads block framing eagerly and would choke on the padding.
- **Privacy:** RAM-only — never extracted to disk, so the no-trace guarantee
  holds (`viewing_a_{zip,7z,tar,tar_gz,rar}_writes_nothing_to_disk`, the RAR one
  covering a decrypt). Errors surface in the egui `Message` dialog.
- **Passwords:** ZIP/7z/RAR5 all prompt in-app; RAR4 and the tar family have no
  in-app decryption.
- **Crates:** `zip` + `sevenz-rust2` + `tar`/`flate2`/`bzip2`/`ruzstd`/`lzma-rust2`
  + `compcol` + `aes`/`cbc`/`hmac`/`sha2` (all pure Rust, no C build risk).
