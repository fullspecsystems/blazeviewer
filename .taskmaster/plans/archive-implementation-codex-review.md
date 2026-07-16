# Codex review of the #102/#103 implementation (2026-07-16)

`codex exec review --base <c385d59>` over the four implementation commits
(d993fb6..1226367). All six findings were verified and fixed in `e532b22`, each
with a regression test; the corpus differential (18 archives, 60 entries
byte-identical to unrar) still passes after the fixes.

Disposition: P1 metadata work-cap -> counted at the MeteredReader choke point;
RAR header CRC -> verified before parsing; solid duplicate-name budget ->
released via upsert accounting; unsafe-named solid members -> kept in the
stream model, excluded from the index only; truncated zstd skippable frame ->
Corrupt; RAR solid-size sums -> saturating.

---

The archive support passes its current tests, but several hostile or damaged archive shapes bypass work limits, checksum validation, or truncation detection. Solid RAR accounting and stream construction can also reject or break otherwise viewable archives.

Full review comments:

- [P1] Count metadata toward the expanded-work limit — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\tar_source.rs:405-405
  For compressed tars containing zero-byte files with GNU long-name or PAX metadata below `META_QUOTA` per entry, `expanded` is incremented only by payload size. Because the metadata quota resets before every `next()`, up to one million entries can inflate and parse hundreds of GiB while `expanded` remains near zero, bypassing the 64 GiB work-bomb cap and tying up the worker. Track all decompressed bytes, including headers, metadata, and padding.

- [P2] Verify each RAR block header checksum — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\rar.rs:429-430
  When a RAR5 block header is corrupted, the first four CRC bytes are read into `pre` but never checked before parsing the name, flags, and sizes. A bit-flipped header can therefore be accepted, potentially serving data under the wrong name or skipping entries instead of reporting a damaged archive. Verify the stored header CRC before interpreting each block.

- [P2] Release replaced RAR entries from the RAM budget — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\rar.rs:699-701
  For a solid RAR containing repeated pathnames, such as an append/update archive, `resident` still includes the previous resident value when the replacement is budget-checked and later inserted into `latest`. Although the old vector is then dropped, its size is never subtracted, so an archive whose final last-wins contents fit can incorrectly fail with `TooLarge`; the stale count also remains when the replacement fails CRC. Account for an existing resident entry of the same name before this check.

- [P2] Preserve skipped RAR members in solid decode chains — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\rar.rs:500-500
  When a regular member has an unsafe or overlong name, this condition drops it from `groups` entirely. If that member seeds or continues a solid group, later supported entries depend on its compressed bytes and LZ state, but `decode_solid_group` concatenates only the retained runs, causing those later images to fail. Keep every regular member in the solid stream model while excluding unsafe names only from the final item index.

- [P2] Reject truncated zstd skippable-frame payloads — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\tar_source.rs:916-916
  When a `.tar.zst` ends with a skippable frame whose declared length exceeds the remaining bytes, `io::copy` returns a short count successfully and that count is ignored. The next frame probe then treats EOF as clean, so the damaged archive opens successfully despite the trailer-validation drain. Require the copied count to equal `length` and report a corrupt stream otherwise.

- [P2] Saturate RAR solid-size totals before enforcing the cap — C:\Users\jdlien\code\blazeviewer-wt1\crates\pb-source\src\rar.rs:560-560
  For a hostile RAR whose solid members' declared unpack sizes overflow `u64` in aggregate, this `sum()` panics with overflow checks enabled and wraps in release. The wrapped value can pass `max_expanded`, and `decode_solid_group` repeats the unchecked sum for the decoder total, leaving the work and progress guards inconsistent with the headers. Use checked or saturating accumulation and reject overflow.
