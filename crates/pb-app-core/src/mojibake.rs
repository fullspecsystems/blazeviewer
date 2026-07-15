//! **Mojibake repair** (task #90.2): text that was already UTF-8, decoded as CP1252, and
//! re-encoded as UTF-8 — the `â™ª` for `♪` defect.
//!
//! Subtitles are the worst-encoded text on a computer. They are hand-made, passed between
//! tools that each guess a charset, and muxed by scripts that guess again. The corpus MKV
//! this was written against carries `c3a2 e284 a2c2 aa` in the container where `e299 aa`
//! (`♪`) belongs — and its own `.eng.srt` sidecar, same content, is clean. Nobody noticed
//! because nothing rendered the embedded track before now.
//!
//! ## Why this is a proof, not a guess
//!
//! Charset *detection* is guesswork. This is not detection. Mojibake is a **lossless,
//! reversible** transform, so the inverse is checkable:
//!
//! ```text
//! "â™ª"  →  chars â(E2) ™(99) ª(AA)  →  bytes E2 99 AA  →  valid UTF-8  →  "♪"   ✔ repair
//! "café" →  chars c a f é(E9)         →  bytes 63 61 66 E9 →  INVALID UTF-8  →  leave alone
//! ```
//!
//! Real text almost never survives the round trip: `é` alone is byte `E9`, a UTF-8 lead
//! that demands a continuation byte, and the next character in ordinary prose is ASCII.
//! **The validity check does the work** — we only ever rewrite a run that was, provably,
//! a well-formed UTF-8 sequence wearing a CP1252 costume.
//!
//! ## The trap this is designed around (read before widening `SAFE_LEADS`)
//!
//! Validity alone is *not* quite enough, and the counter-example is real French:
//!
//! ```text
//! "voilà «"  →  à(E0) NBSP(A0) «(AB)  →  bytes E0 A0 AB  →  VALID UTF-8  →  "ࠫ" (U+082B)
//! ```
//!
//! Perfectly ordinary typography — an a-grave, a non-breaking space, a guillemet — is a
//! well-formed three-byte sequence, and a naive round trip silently turns it into a
//! Samaritan letter. So a run is repaired only when it *begins* with one of the few leads
//! that real mojibake actually starts with ([`SAFE_LEADS`]), which excludes `à`/`á`.
//!
//! The cost of that conservatism is honest: mojibake'd Thai or Devanagari (lead `à`) is
//! left broken. That is the right trade — French, Spanish, and Portuguese subtitles are
//! everywhere, and corrupting correct text is far worse than failing to fix broken text.
//!
//! ## Isolation
//!
//! Repair is applied **per run of consecutive non-ASCII characters**, never to the string
//! as a whole. A mojibake sequence is entirely non-ASCII by construction (leads are
//! `C2..F4`, continuations `80..BF` — none are ASCII), so runs are exactly the right unit,
//! and one unfixable run can never drag a fixable neighbour down with it. `"café â™ª"`
//! repairs the `â™ª` and leaves the `é` alone.

use std::borrow::Cow;

/// The CP1252 bytes that real mojibake starts with — i.e. the UTF-8 lead bytes whose
/// target ranges are the ones subtitles actually contain.
///
/// | char | byte | recovers | why it's in |
/// |---|---|---|---|
/// | `Â` | C2 | U+0080–U+00BF | `Â¿`, `Â°`, `Â ` (nbsp) — the punctuation mojibake |
/// | `Ã` | C3 | U+00C0–U+00FF | `Ã©`, `Ã±` — every accented Latin letter |
/// | `â` | E2 | U+2000–U+2FFF | smart quotes, dashes, `â™ª` — **the most common of all** |
/// | `ã` | E3 | U+3000–U+3FFF | CJK punctuation + kana |
/// | `ï` | EF | U+F000–U+FFFF | a mojibake'd BOM, CJK compatibility forms |
/// | `ð` | F0 | U+10000+ | emoji |
///
/// Deliberately **absent**: `à` (E0) and `á` (E1). They lead U+0800–U+1FFF (Thai,
/// Devanagari, Georgian…), and they are also two of the most common letters in French,
/// Spanish, Portuguese, and Italian — which is exactly the `voilà «` collision in the
/// module docs. Do not add them without a stronger plausibility check than validity.
const SAFE_LEADS: &[char] = &['Â', 'Ã', 'â', 'ã', 'ï', 'ð'];

/// How many times to re-run the repair.
///
/// Mojibake compounds: text can be round-tripped through the wrong charset more than
/// once, and each pass peels one layer. `'` (U+2019) double-encoded is `â€™`;
/// triple-encoded it is `Ã¢â‚¬â„¢`. Three passes covers every layering seen in the wild,
/// and the loop exits as soon as a pass changes nothing.
const MAX_PASSES: usize = 3;

/// Undo double-encoded UTF-8, if — and only if — it provably *is* double-encoded.
///
/// Borrows on the overwhelmingly common path (ASCII, or clean text with no repairable
/// run), so calling it on every cue of every subtitle costs a scan and no allocation.
pub fn repair(s: &str) -> Cow<'_, str> {
    // The fast path out: a mojibake sequence is entirely non-ASCII, so pure-ASCII text —
    // most English subtitles, every line of them — can exit on a memchr-speed scan.
    if s.is_ascii() {
        return Cow::Borrowed(s);
    }
    let mut cur = Cow::Borrowed(s);
    for _ in 0..MAX_PASSES {
        match repair_once(&cur) {
            // Nothing left to peel.
            None => break,
            Some(next) => cur = Cow::Owned(next),
        }
    }
    cur
}

/// One peel. `None` when no run was repairable, so the caller can stop and the borrow
/// survives.
fn repair_once(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    let mut rest = s;

    while !rest.is_empty() {
        // Copy the ASCII up to the next run verbatim.
        let run_start = rest
            .char_indices()
            .find(|(_, c)| !c.is_ascii())
            .map(|(i, _)| i);
        let Some(start) = run_start else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // The maximal run of non-ASCII characters.
        let run_len = after
            .char_indices()
            .find(|(_, c)| c.is_ascii())
            .map_or(after.len(), |(i, _)| i);
        let (run, tail) = after.split_at(run_len);

        match repair_run(run) {
            Some(fixed) => {
                out.push_str(&fixed);
                changed = true;
            }
            None => out.push_str(run),
        }
        rest = tail;
    }
    changed.then_some(out)
}

/// One run of non-ASCII characters → its repair, or `None` to leave it exactly as it is.
///
/// Every `None` below is a guard earning its keep; none of them is defensive padding.
fn repair_run(run: &str) -> Option<String> {
    // Guard 1 — the lead. The `voilà «` defence. See `SAFE_LEADS`.
    if !SAFE_LEADS.contains(&run.chars().next()?) {
        return None;
    }
    // Guard 2 — every character must be one CP1252 byte. A character outside CP1252
    // (say a real `♪`, already correct) means this run was never a byte sequence.
    let bytes: Option<Vec<u8>> = run.chars().map(cp1252_byte).collect();
    let bytes = bytes?;

    // Guard 3 — THE proof. If these bytes are not valid UTF-8, the run was never
    // double-encoded text and we must not touch it.
    let decoded = std::str::from_utf8(&bytes).ok()?;

    // Guard 4 — a repair that recovers a control character decoded garbage, not text
    // (`Â` + `€` is bytes C2 80 = U+0080, a C1 control). Real subtitles have none.
    if decoded
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return None;
    }
    // Guard 5 — a no-op "repair" is not a repair; returning it would spin the pass loop.
    (decoded != run).then(|| decoded.to_string())
}

/// A character → its single CP1252 byte, or `None` if CP1252 cannot represent it.
///
/// CP1252 is Latin-1 **except** for `0x80..=0x9F`, where Latin-1 has unused C1 controls
/// and CP1252 puts the typographic characters. That range is the whole reason this
/// function exists rather than a cast: `™` is `0x99` in CP1252 and unrepresentable in
/// Latin-1, and `™` is precisely the character standing in the middle of `â™ª`. A
/// Latin-1-only implementation would fail on the exact case that motivated this module.
fn cp1252_byte(c: char) -> Option<u8> {
    // The C1 range, where CP1252 diverges from Latin-1.
    //
    // ⚠ The five bytes 0x81/0x8D/0x8F/0x90/0x9D are *unassigned* in IBM's CP1252 — and
    // are mapped straight through to their C1 controls by the **WHATWG windows-1252
    // standard**, which is what browsers and essentially every real decoder implement.
    // Real mojibake therefore contains them, and omitting them is not "strict", it is
    // broken: `”` (U+201D) is bytes E2 80 **9D**, so a table without 0x9D fails to repair
    // every closing curly quote in existence. (Found by a test, not by reading the spec.)
    // Guard 4 in `repair_run` is what keeps these safe — a repair whose *result* is a
    // control character is still rejected.
    const HIGH: &[(char, u8)] = &[
        ('\u{20AC}', 0x80), // €
        ('\u{0081}', 0x81), // WHATWG passthrough
        ('\u{008D}', 0x8D), // WHATWG passthrough
        ('\u{008F}', 0x8F), // WHATWG passthrough
        ('\u{0090}', 0x90), // WHATWG passthrough
        ('\u{009D}', 0x9D), // WHATWG passthrough — the closing curly quote's third byte
        ('\u{201A}', 0x82), // ‚
        ('\u{0192}', 0x83), // ƒ
        ('\u{201E}', 0x84), // „
        ('\u{2026}', 0x85), // …
        ('\u{2020}', 0x86), // †
        ('\u{2021}', 0x87), // ‡
        ('\u{02C6}', 0x88), // ˆ
        ('\u{2030}', 0x89), // ‰
        ('\u{0160}', 0x8A), // Š
        ('\u{2039}', 0x8B), // ‹
        ('\u{0152}', 0x8C), // Œ
        ('\u{017D}', 0x8E), // Ž
        ('\u{2018}', 0x91), // '
        ('\u{2019}', 0x92), // '
        ('\u{201C}', 0x93), // "
        ('\u{201D}', 0x94), // "
        ('\u{2022}', 0x95), // •
        ('\u{2013}', 0x96), // –
        ('\u{2014}', 0x97), // —
        ('\u{02DC}', 0x98), // ˜
        ('\u{2122}', 0x99), // ™
        ('\u{0161}', 0x9A), // š
        ('\u{203A}', 0x9B), // ›
        ('\u{0153}', 0x9C), // œ
        ('\u{017E}', 0x9E), // ž
        ('\u{0178}', 0x9F), // Ÿ
    ];
    let u = c as u32;
    // Latin-1 agrees with CP1252 outside the C1 range...
    if u < 0x80 || (0xA0..=0xFF).contains(&u) {
        return Some(u as u8);
    }
    // ...and a raw C1 control is NOT a CP1252 character. Accepting it would let a run of
    // genuine control bytes masquerade as text.
    HIGH.iter().find(|(hc, _)| *hc == c).map(|(_, b)| *b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus defect this module was written for: a real MKV's embedded subrip track
    /// carries the music note double-encoded, while its sidecar carries it clean.
    #[test]
    fn the_greys_anatomy_music_note_is_repaired() {
        assert_eq!(repair("â™ª WAKE UP â™ª"), "♪ WAKE UP ♪");
    }

    /// THE false positive that matters. `é` alone is byte E9 — a UTF-8 lead with no
    /// continuation — so the proof fails and the text is untouched.
    #[test]
    fn ordinary_accented_text_is_never_touched() {
        for s in [
            "café",
            "naïve",
            "Björk",
            "señor",
            "Ärzte",
            "São Paulo",
            "¿Qué?",
            "Grüße",
        ] {
            assert_eq!(repair(s), s, "{s} must survive untouched");
            assert!(
                matches!(repair(s), Cow::Borrowed(_)),
                "{s} must not allocate"
            );
        }
    }

    /// The `voilà «` trap, spelled out. a-grave + NBSP + guillemet IS valid UTF-8 when
    /// round-tripped (it decodes to U+082B, a Samaritan letter). Only the `SAFE_LEADS`
    /// guard saves it — if this test fails, someone widened that set.
    #[test]
    fn french_guillemets_are_not_mistaken_for_mojibake() {
        let s = "voilà\u{a0}« bonjour »";
        assert_eq!(repair(s), s);
        // Prove the trap is real rather than hypothetical: the bytes DO round-trip.
        let bytes: Vec<u8> = "à\u{a0}«"
            .chars()
            .map(|c| cp1252_byte(c).unwrap())
            .collect();
        assert_eq!(bytes, vec![0xE0, 0xA0, 0xAB]);
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "\u{82B}");
    }

    /// The most common mojibake in real subtitles by a distance: smart quotes.
    ///
    /// Escapes, not literals: the whole subject here is characters that look alike and
    /// are not, so an expectation typed as an ASCII `'` would assert the wrong thing.
    #[test]
    fn smart_punctuation_is_repaired() {
        assert_eq!(repair("Iâ€™m fine"), "I\u{2019}m fine"); // ’
        assert_eq!(repair("â€œquotedâ€\u{9d}"), "\u{201c}quoted\u{201d}"); // “ ”
        assert_eq!(repair("dashâ€”here"), "dash\u{2014}here"); // —
    }

    #[test]
    fn accented_mojibake_is_repaired() {
        assert_eq!(repair("CafÃ©"), "Café");
        assert_eq!(repair("seÃ±or"), "señor");
        assert_eq!(repair("Â¿QuÃ©?"), "¿Qué?");
    }

    /// Mojibake compounds — each pass peels one layer.
    #[test]
    fn multiply_encoded_text_peels_every_layer() {
        // U+2019 (’) encoded twice, then three times. Both land on the same character.
        assert_eq!(repair("â€™"), "\u{2019}");
        assert_eq!(repair("Ã¢â‚¬â„¢"), "\u{2019}");
    }

    /// Runs are independent: a broken one is fixed without disturbing a correct neighbour
    /// in the same line.
    #[test]
    fn a_broken_run_is_fixed_without_touching_a_correct_one() {
        assert_eq!(repair("café â™ª"), "café ♪");
        assert_eq!(repair("â™ª señor â™ª"), "♪ señor ♪");
    }

    /// Text that is already correct is a fixed point. Running repair twice must equal
    /// running it once — otherwise a re-parse could corrupt clean text.
    #[test]
    fn repair_is_idempotent() {
        for s in ["♪ WAKE UP ♪", "café", "I'm fine", "日本語", "Привет", "🎬"] {
            assert_eq!(repair(s), s);
            let once = repair(s).into_owned();
            assert_eq!(repair(&once), once);
        }
    }

    /// A C1 control is not text. `Â` + `€` round-trips to U+0080 and must be rejected.
    #[test]
    fn a_repair_that_recovers_a_control_char_is_rejected() {
        assert_eq!(repair("Â\u{20ac}"), "Â\u{20ac}");
    }

    /// CP1252's C1 range is the whole point — a Latin-1-only map would miss `™`, which
    /// stands in the middle of the very sequence this module was written for.
    #[test]
    fn the_cp1252_c1_range_is_mapped_not_latin1() {
        assert_eq!(cp1252_byte('™'), Some(0x99));
        assert_eq!(cp1252_byte('€'), Some(0x80));
        assert_eq!(cp1252_byte('œ'), Some(0x9C));
        // Outside CP1252 entirely — these are already-correct characters, and mapping
        // them is what guard 2 exists to refuse.
        assert_eq!(cp1252_byte('♪'), None);
        assert_eq!(cp1252_byte('日'), None);
    }

    /// The WHATWG passthroughs. IBM leaves these five unassigned; every real decoder maps
    /// them to C1 controls, so real mojibake contains them. `”` is the case that proves
    /// it: byte 0x9D is its third byte, and without this the closing quote of every
    /// quoted line stays broken while the opening one gets fixed.
    #[test]
    fn the_five_whatwg_passthrough_bytes_are_mapped() {
        for (c, b) in [
            ('\u{81}', 0x81u8),
            ('\u{8d}', 0x8d),
            ('\u{8f}', 0x8f),
            ('\u{90}', 0x90),
            ('\u{9d}', 0x9d),
        ] {
            assert_eq!(cp1252_byte(c), Some(b), "U+{:04X} must map", c as u32);
        }
        // The pair, end to end — an opening quote that repairs while the closing one
        // does not is the exact bug this prevents.
        assert_eq!(repair("â€œhiâ€\u{9d}"), "\u{201c}hi\u{201d}");
    }

    #[test]
    fn ascii_borrows_and_never_allocates() {
        assert!(matches!(repair("plain ascii"), Cow::Borrowed(_)));
        assert!(matches!(repair(""), Cow::Borrowed(_)));
    }

    /// Non-Latin scripts that are already correct must survive — they are not CP1252
    /// characters, so guard 2 stops them before the round trip.
    #[test]
    fn correct_non_latin_text_survives() {
        for s in ["日本語字幕", "Привет мир", "مرحبا", "안녕하세요"] {
            assert_eq!(repair(s), s);
        }
    }

    /// The documented, accepted loss: a mojibake'd `à`-lead script is left broken rather
    /// than risk the French collision. Pinned so the trade-off is a decision, not a bug.
    #[test]
    fn mojibaked_thai_is_deliberately_left_alone() {
        // "à¸ª" would be U+0E2A (Thai) if repaired. We decline, on purpose.
        assert_eq!(repair("à¸ª"), "à¸ª");
    }
}
