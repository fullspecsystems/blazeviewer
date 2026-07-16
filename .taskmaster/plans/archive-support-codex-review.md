The overall architecture is sound, but I would not execute the plan as written yet. There are several correctness and OOM-safety gaps worth fixing in a rev2.

## Findings

1. **[P0] Plain TAR indexing must use seek-aware iteration and should not run on the event loop.**

   The plan says `tar::Archive::entries()` and treats plain TAR like synchronous ZIP ([lines 287–308](C:/Users/jdlien/code/blazeviewer-wt1/.taskmaster/plans/102-tar-family-archives.md:287), [lines 345–355](C:/Users/jdlien/code/blazeviewer-wt1/.taskmaster/plans/102-tar-family-archives.md:345)). Ordinary `entries()` reads/discards every payload byte. The crate has a separate `entries_with_seek()` specifically to skip file contents efficiently. Even then, indexing is O(entry count) filesystem I/O and can freeze the UI on a huge/network archive. [The tar API documents this distinction](https://docs.rs/tar/latest/tar/struct.Archive.html).

   Incorporate:

   - Require `entries_with_seek()` for plain TAR.
   - Open every TAR variant off-thread; “lazy source” does not imply “cheap synchronous open.”
   - Replace `ArchiveKind::eager()` with something like `requires_background_open()`. Access model and open scheduling are different concepts.
   - Prefer a central `scan::load_archive(kind, ...)` worker entry point so both shells stop duplicating dispatch.
   - This also fits RAR5: solidness cannot be determined from the `.rar` suffix, so a static `eager()` API does not model task 103 correctly.

2. **[P0] The proposed budget enforcement crosses the crate boundary incorrectly, and Linux currently has no real RAM query.**

   `TarSource` lives in `pb-source`, while `archive::ram_budget()` lives in `pb-app-core`; `pb-source` cannot call upward without a dependency cycle. The plan does not show how the source receives the budget ([lines 325–338](C:/Users/jdlien/code/blazeviewer-wt1/.taskmaster/plans/102-tar-family-archives.md:325)).

   Incorporate:

   - Add `resident_budget: u64` to the eager source/open call.
   - Add `OpenError::TooLarge { needed, budget }` in `pb-source`, then map it explicitly to `ArchiveOpenError::TooLarge`.
   - Use `checked_add` for resident accounting.
   - Update `ArchiveOpenError::TooLarge` wording because refusal may now happen mid-stream and `needed` is only “at least.”

   More importantly, Linux currently returns `None` for available RAM and falls back to an assumed 8 GB ([archive.rs](C:/Users/jdlien/code/blazeviewer-wt1/crates/pb-app-core/src/archive.rs:99)). That undermines the safety gate on the platform this feature targets first. Add a tested Linux `MemAvailable` reader from `/proc/meminfo` before shipping eager tarballs.

3. **[P0] Normal `tar` iteration can allocate unbounded PAX/GNU metadata before your guards run.**

   The plan relies on `tar` handling PAX/GNU names and applies `try_reserve` only to returned file entries. Internally, the crate reads GNU long-name and PAX extension entries into a `Vec` using `read_to_end`; its 128 KiB cap is only the initial capacity ([installed tar source](C:/Users/jdlien/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tar-0.4.46/src/entry.rs:297)). A hostile metadata header can therefore trigger an uncatchable allocation failure before `TarSource` sees the entry. Sparse parsing also allocates internally.

   Incorporate one of these explicit strategies:

   - Use raw iteration and process bounded GNU/PAX metadata yourself; or
   - Use/pin a tar implementation patched with fallible metadata limits.

   Also add:

   - Maximum metadata bytes and path length.
   - Maximum entry count or total index-table budget.
   - Fallible reserves for entry/name/dedup tables.
   - Plain-TAR validation that `offset.checked_add(size) <= file_len`.
   - Exact reads in `bytes(i)`, so truncated payloads cannot be returned as valid.

4. **[P0] `.tar.zst` is not ready behind a simple `ruzstd::StreamingDecoder`.**

   `StreamingDecoder` explicitly handles only one frame; callers must implement concatenated frames and skippable frames themselves. [Ruzstd documents that caveat](https://docs.rs/ruzstd/latest/ruzstd/decoding/struct.StreamingDecoder.html).

   The installed 0.8.3 source also has two safety/integrity concerns:

   - The advertised 100 MB window limit is applied on reset, but not when constructing the first frame state ([frame_decoder.rs](C:/Users/jdlien/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ruzstd-0.8.3/src/decoding/frame_decoder.rs:96)).
   - The streaming wrapper does not compare the stored zstd checksum with the calculated checksum.

   Make `.tar.zst` a gated phase:

   - Multi-frame and skippable-frame support.
   - Checksum verification for every frame.
   - A proven pre-allocation window cap, including the first frame.
   - Tests for concatenated frames, skippable frames, checksum corruption, dictionaries, huge windows, and truncation.

   If those cannot be guaranteed safely, defer `.tar.zst` rather than shipping the current reader shape.

5. **[P1] Stream completion, truncation, and decompression-work limits need a more precise design.**

   The reused `read_cancellable` currently treats early EOF as success ([lib.rs](C:/Users/jdlien/code/blazeviewer-wt1/crates/pb-source/src/lib.rs:1085)), which conflicts with the planned “truncated stream → `Corrupt`” behavior.

   Also, the tar parser stops at the end-of-archive zero header before the compression reader necessarily reaches EOF. Unless the decoder is subsequently drained:

   - gzip/bzip2 trailers and checksums may not be validated;
   - trailing corruption may be missed;
   - compressed-byte progress may never reach completion.

   Incorporate:

   - A new exact cancellable read helper that returns `UnexpectedEof`.
   - Drain the compression reader to EOF after tar iteration, under cancellation and output limits.
   - Map `InvalidData`/`UnexpectedEof` to `Corrupt`, while preserving genuine filesystem errors as `Io`.
   - Track total expanded work, including unsupported and oversized entries. Resident-byte accounting alone allows a tiny archive to force petabytes of skip-and-drain CPU work.
   - Do not claim 64 KiB strictly bounds cancellation latency; it bounds checks between output reads, while a decoder may process a whole internal block before returning.
   - Add an explicit `mark_complete()` concept so the UI does not sit at 100% while decompression continues.

   Use `MultiGzDecoder` and `MultiBzDecoder`, not their single-stream variants; both crates explicitly distinguish these APIs: [flate2](https://docs.rs/flate2/latest/flate2/read/struct.MultiGzDecoder.html), [bzip2](https://docs.rs/bzip2/latest/bzip2/).

6. **[P1] The picker plan makes the headline formats unavailable.**

   The plan says not to add `gz`, `bz2`, or `zst` until optional phase 5 ([lines 357–360](C:/Users/jdlien/code/blazeviewer-wt1/.taskmaster/plans/102-tar-family-archives.md:357)). File pickers filter on the final extension, so `.tar.gz`, `.tar.bz2`, and `.tar.zst` require exactly those suffixes. On macOS there is no “All files” escape hatch in the current panel ([CoreModel.swift](C:/Users/jdlien/code/blazeviewer-wt1/mac/Sources/BlazeViewerMac/CoreModel.swift:2377)).

   Resolve this by either:

   - Making bare compressed images part of the required release and adding those suffixes; or
   - Adding the suffixes in phase 4 and accepting that unrelated bare compressed files can appear and be rejected cleanly.

   Also add `tbz` to the picker if the classifier recognizes it.

   Remove `.svgz` from phase 5. It is already a supported ordinary image with bounded gzip inflation ([svg.rs](C:/Users/jdlien/code/blazeviewer-wt1/crates/pb-decode/src/svg.rs:71)). Reclassifying it as an archive would regress normal folder browsing and filesystem metadata behavior.

7. **[P2] Define a strict virtual-path policy.**

   `normalize_entry_name` only changes separators and strips leading `./` or `/`. Add a shared archive-name normalizer that rejects or handles:

   - `..` components;
   - absolute/root/drive-prefixed names;
   - empty or NUL-containing names;
   - overlong names;
   - non-UTF-8 tar names, with a deliberate display/extension-routing policy.

   Sparse entries must be explicitly rejected before using `raw_file_position`; that API guarantees contiguous raw bytes only for non-sparse files. [The tar entry documentation states this caveat](https://docs.rs/tar/latest/tar/struct.Entry.html).

   Hard links can absolutely represent image names, so change “they can’t be images” to either a documented limitation or implement safe internal aliasing. Symlinks can reasonably remain skipped.

8. **[P2] The wiring inventory and phase text are stale.**

   The final `grep ... crates/` audit misses Swift and packaging surfaces. The relevant inventory includes:

   - Winit picker and classifier.
   - Rust macOS FFI classifier.
   - Swift open-panel extensions.
   - macOS Info.plist archive UTIs.
   - Windows optional archive associations in [default_app.rs](C:/Users/jdlien/code/blazeviewer-wt1/crates/pb-app/src/default_app.rs:136).
   - CLI/help text and comments.

   Explicitly mark OS associations as intentionally unchanged. “Check Info.plist” and “do not add associations” currently leave the outcome ambiguous.

   Phase 0 also still says to rerun spike 102.0 even though the same document records completed findings. Convert those findings into fixed decisions. The locally installed `lzma-rust2` already has `XzReader` with multi-stream support, so `.tar.xz` viability is no longer an API question; its remaining gate is a safe decoder-memory limit.

## Test and measurement additions

Before execution, add these to the plan:

- Fuzz target covering raw tar parsing plus each bounded compression wrapper.
- PAX/GNU-long-name metadata bomb and million-entry/zero-byte-table bomb.
- Truncated last payload in plain TAR.
- Multi-member gzip/bzip2 and multi-frame/skippable zstd.
- Corrupt codec checksum/trailer.
- Huge decoder-window/dictionary requests.
- Total expanded-work refusal.
- Non-UTF-8 and traversal-shaped names.
- Concurrent `bytes(i)` reads from lazy TAR.
- Exact progress completion and monotonicity.
- Benchmarks for plain-TAR index latency, codec throughput, cancellation latency, and peak RSS on representative corpus sizes.

Verdict: the source seam and lazy/eager split are the right foundation. I recommend revising the plan to rev2 around these eight findings—especially background opening, bounded tar metadata parsing, Linux RAM detection, and the zstd gate—before implementation begins. No files were changed.
