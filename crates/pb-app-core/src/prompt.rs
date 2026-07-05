//! AI image-description prompt builder (task #44, subtask 2).
//!
//! Pure functions that turn a photo's salient metadata into the prompt sent to the
//! vision model. Two stages so each is a clean [TDD] target:
//!
//! 1. [`build_context`] — **salience + hygiene.** Pick the few EXIF facts worth
//!    giving a describer (capture time, camera/lens, GPS), plus the filename and
//!    parent folder, from the flat `(tag, value)` list the info panel already reads
//!    (`pb_decode::read_exif_fields`). Junk timestamps (epoch defaults, future
//!    dates) are dropped; GPS is normalized toward decimal coordinates.
//! 2. [`build_prompt`] — **templating.** Substitute the context into the default
//!    instruction (or a user-supplied `settings.toml` template) so **both backends
//!    (Apple Foundation Models and the local endpoint) send an identical prompt**.
//!
//! The whole file is `std`-only and deterministic — no I/O, no clock (the "now" used
//! to reject future capture dates is passed in) — so it unit-tests exhaustively.
//!
//! Framing note (the "wedding-photographer clock" problem): metadata is presented to
//! the model as **unverified** and explicitly overridable by the pixels. A camera set
//! to the wrong time zone, a scanned photo dated by its scan, a mis-tagged folder —
//! none should make the model contradict what it can see.

/// The salient, already-cleaned metadata that seeds a description prompt — the output
/// of [`build_context`] and the input to [`build_prompt`]. Every field is optional and
/// only `Some` when a real, non-junk value was found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DescribeContext {
    /// File name only (never the full path — a path can leak a directory tree, and the
    /// folder is carried separately and deliberately).
    pub filename: Option<String>,
    /// Parent folder name only (not the path above it).
    pub folder: Option<String>,
    /// Capture time, normalized to `YYYY-MM-DD HH:MM:SS`, with junk timestamps dropped.
    pub datetime: Option<String>,
    /// Camera make + model (de-duplicated) with the lens appended when present.
    pub camera: Option<String>,
    /// Location as decimal coordinates (`"43.65241, -79.38790"`) when the GPS tags parse,
    /// else the raw coordinate strings labeled as such. Reverse-geocoding to a place name
    /// is a later upgrade (see the plan doc).
    pub location: Option<String>,
}

impl DescribeContext {
    /// The labeled context lines (`Filename: …`, `Taken: …`, …), in the plan's order,
    /// each present only when its field is. Empty when nothing salient was found.
    fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push = |label: &str, val: &Option<String>| {
            if let Some(v) = val {
                out.push(format!("{label}: {v}"));
            }
        };
        push("Filename", &self.filename);
        push("Folder", &self.folder);
        push("Taken", &self.datetime);
        push("Camera", &self.camera);
        push("Location", &self.location);
        out
    }

    /// The assembled `{context}` block — labeled lines joined by newlines. Empty string
    /// when there's nothing salient (so the default prompt can omit the whole trailer).
    pub fn block(&self) -> String {
        self.lines().join("\n")
    }

    /// True when no salient metadata was found at all — the prompt is then the bare
    /// instruction, and the model works from the pixels alone.
    pub fn is_empty(&self) -> bool {
        self.filename.is_none()
            && self.folder.is_none()
            && self.datetime.is_none()
            && self.camera.is_none()
            && self.location.is_none()
    }
}

/// The default instruction — the accessibility brief. Kept free of the context trailer so
/// the "no metadata" case is the bare instruction (see [`build_prompt`]).
pub const DEFAULT_INSTRUCTION: &str = "\
Describe this image for someone who cannot see it. Lead with the subject and \
setting in one sentence, then notable details (people, text, colors, mood). \
Be concrete and concise — 2 to 4 sentences. Describe only what is visible.";

/// The preamble that introduces the metadata block in the default prompt, framing every
/// fact as unverified (the hallucination guard).
pub const CONTEXT_PREAMBLE: &str = "\
Context — file metadata, which MAY BE WRONG or irrelevant; trust the pixels over \
it and ignore anything that conflicts with what you see:";

/// Build the salient [`DescribeContext`] from the raw inputs the core already has.
///
/// - `name` is the item's display name / relative path; only its file component is kept,
///   and its parent directory name becomes the folder.
/// - `exif` is the flat `(tag, value)` list from `pb_decode::read_exif_fields`
///   (memoized in `AppCore::exif_cache`) — no new parsing.
/// - `now` is `(year, month, day)` used only to reject future capture dates; pass `None`
///   to skip that check (tests, or if a clock isn't handy).
pub fn build_context(
    name: &str,
    exif: &[(String, String)],
    now: Option<(i32, u32, u32)>,
) -> DescribeContext {
    let (folder, filename) = split_name(name);
    DescribeContext {
        filename,
        folder,
        datetime: salient_datetime(exif, now),
        camera: salient_camera(exif),
        location: salient_location(exif),
    }
}

/// Build the final prompt string. `template` is the advanced `settings.toml` override
/// (`describe_prompt`); `None` uses the default instruction plus the context trailer.
///
/// A custom template is substituted verbatim through the placeholder set
/// (`{context}`, `{filename}`, `{folder}`, `{datetime}`, `{camera}`, `{location}`) —
/// an absent field becomes an empty string, so the author controls the whole layout.
/// The default path composes [`DEFAULT_INSTRUCTION`] + [`CONTEXT_PREAMBLE`] + the block,
/// and drops the trailer entirely when there's no metadata.
pub fn build_prompt(ctx: &DescribeContext, template: Option<&str>) -> String {
    match template {
        Some(t) => substitute(t, ctx),
        None => {
            if ctx.is_empty() {
                DEFAULT_INSTRUCTION.to_string()
            } else {
                format!(
                    "{DEFAULT_INSTRUCTION}\n\n{CONTEXT_PREAMBLE}\n{}",
                    ctx.block()
                )
            }
        }
    }
}

/// Substitute the placeholder set into a custom template. `{context}` expands to the whole
/// labeled block; the individual placeholders expand to their field (empty when absent).
fn substitute(template: &str, ctx: &DescribeContext) -> String {
    let empty = String::new();
    template
        .replace("{context}", &ctx.block())
        .replace("{filename}", ctx.filename.as_ref().unwrap_or(&empty))
        .replace("{folder}", ctx.folder.as_ref().unwrap_or(&empty))
        .replace("{datetime}", ctx.datetime.as_ref().unwrap_or(&empty))
        .replace("{camera}", ctx.camera.as_ref().unwrap_or(&empty))
        .replace("{location}", ctx.location.as_ref().unwrap_or(&empty))
}

/// Split a display name / relative path into `(parent folder name, file name)`. Handles
/// both `/` and `\` separators (an archive entry or a Windows path may carry either).
fn split_name(name: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = name
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let filename = parts.last().map(|s| s.to_string());
    // The folder is the segment just above the file, when there is one.
    let folder = if parts.len() >= 2 {
        Some(parts[parts.len() - 2].to_string())
    } else {
        None
    };
    (folder, filename)
}

/// First matching tag's value (case-insensitive on the tag name), trimmed and non-empty.
fn field<'a>(exif: &'a [(String, String)], tag: &str) -> Option<&'a str> {
    exif.iter()
        .find(|(t, _)| t.eq_ignore_ascii_case(tag))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
}

/// Capture time from `DateTimeOriginal` (falling back to `DateTime`), normalized and
/// junk-filtered. Returns `None` for a missing, unparseable, epoch-default, or future date.
fn salient_datetime(exif: &[(String, String)], now: Option<(i32, u32, u32)>) -> Option<String> {
    let raw = field(exif, "DateTimeOriginal").or_else(|| field(exif, "DateTime"))?;
    let dt = parse_exif_datetime(raw)?;
    if is_junk_datetime(dt, now) {
        return None;
    }
    let (y, mo, d, h, mi, s) = dt;
    Some(format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"))
}

/// Parse an EXIF datetime (`"YYYY:MM:DD HH:MM:SS"`, the standard form) into its
/// components. Tolerant of a missing time (`"YYYY:MM:DD"` → midnight). `None` if the
/// date part doesn't parse.
fn parse_exif_datetime(raw: &str) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let mut halves = raw.trim().splitn(2, [' ', 'T']);
    let date = halves.next()?;
    let time = halves.next().unwrap_or("00:00:00");
    let mut d = date.split([':', '-', '/']);
    let y: i32 = d.next()?.trim().parse().ok()?;
    let mo: u32 = d.next()?.trim().parse().ok()?;
    let da: u32 = d.next()?.trim().parse().ok()?;
    let mut t = time.split([':', '.']);
    let h: u32 = t.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let mi: u32 = t.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    let se: u32 = t.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    // Reject impossible dates outright (0000-00-00 and the like).
    if !(1..=9999).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    Some((y, mo, da, h, mi, se))
}

/// A capture time worth dropping: the camera/scanner epoch-default stamps (1970/1980/2000
/// -01-01 at midnight) that mean "clock never set", or a date in the future relative to
/// `now` (a mis-set clock — a photo can't be taken after today).
fn is_junk_datetime(dt: (i32, u32, u32, u32, u32, u32), now: Option<(i32, u32, u32)>) -> bool {
    let (y, mo, d, h, mi, s) = dt;
    let epoch_default = (mo, d, h, mi, s) == (1, 1, 0, 0, 0) && matches!(y, 1970 | 1980 | 2000);
    let future = now.is_some_and(|n| (y, mo, d) > n);
    epoch_default || future
}

/// Camera make + model (de-duplicated — many bodies store the make inside the model,
/// e.g. `NIKON` / `NIKON D850`), with the lens appended when present.
fn salient_camera(exif: &[(String, String)]) -> Option<String> {
    let make = field(exif, "Make");
    let model = field(exif, "Model");
    let body = match (make, model) {
        (Some(make), Some(model)) => {
            // Drop the make when the model already carries the brand — compared on the
            // make's first token (case-insensitive), since the make is often the verbose
            // legal name ("NIKON CORPORATION") while the model uses the brand ("NIKON D850").
            let brand = make.split_whitespace().next().unwrap_or(make);
            if model
                .to_ascii_lowercase()
                .starts_with(&brand.to_ascii_lowercase())
            {
                model.to_string()
            } else {
                format!("{make} {model}")
            }
        }
        (Some(one), None) | (None, Some(one)) => one.to_string(),
        (None, None) => return None,
    };
    let lens = field(exif, "LensModel").or_else(|| field(exif, "LensSpecification"));
    Some(match lens {
        Some(lens) if !body.contains(lens) => format!("{body}, {lens} lens"),
        _ => body,
    })
}

/// Location from the GPS tags. Prefers decimal degrees parsed from the coordinate values
/// (signed by their N/S/E/W reference); falls back to the raw labeled strings if the
/// values don't parse (kamadak's display format varies by field type).
fn salient_location(exif: &[(String, String)]) -> Option<String> {
    let lat_raw = field(exif, "GPSLatitude")?;
    let lon_raw = field(exif, "GPSLongitude")?;
    let lat_ref = field(exif, "GPSLatitudeRef").unwrap_or("");
    let lon_ref = field(exif, "GPSLongitudeRef").unwrap_or("");
    match (
        coord_to_decimal(lat_raw, lat_ref),
        coord_to_decimal(lon_raw, lon_ref),
    ) {
        (Some(lat), Some(lon)) => Some(format!("{lat:.5}, {lon:.5}")),
        // Couldn't parse — hand the model the raw coordinates, labeled by hemisphere.
        _ => Some(format!(
            "{} {}, {} {}",
            lat_raw.trim(),
            lat_ref,
            lon_raw.trim(),
            lon_ref
        )),
    }
}

/// Parse a GPS coordinate value into signed decimal degrees. Accepts decimal degrees
/// (`"43.65"`) or degrees/minutes/seconds — however the pieces are punctuated — by
/// pulling the leading numbers out of the string: one number is decimal degrees, two or
/// three are D/M(/S). The `ref` (`S`/`W`, case-insensitive) negates the sign.
fn coord_to_decimal(value: &str, hemisphere: &str) -> Option<f64> {
    let nums: Vec<f64> = value
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<f64>().ok())
        .collect();
    let deg = match nums.as_slice() {
        [d] => *d,
        [d, m] => d + m / 60.0,
        [d, m, s, ..] => d + m / 60.0 + s / 3600.0,
        [] => return None,
    };
    if !deg.is_finite() || deg.abs() > 180.0 {
        return None;
    }
    let sign = if matches!(hemisphere.trim().to_ascii_uppercase().as_str(), "S" | "W") {
        -1.0
    } else {
        1.0
    };
    Some(sign * deg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(t, v)| (t.to_string(), v.to_string()))
            .collect()
    }

    // --- split_name / filename + folder --------------------------------------------

    #[test]
    fn split_name_keeps_file_and_parent_only() {
        assert_eq!(
            split_name("trip/rome/DSC_0001.jpg"),
            (Some("rome".to_string()), Some("DSC_0001.jpg".to_string()))
        );
        // Bare file name: no folder.
        assert_eq!(
            split_name("photo.png"),
            (None, Some("photo.png".to_string()))
        );
        // Windows separators inside an archive entry / path.
        assert_eq!(
            split_name(r"album\sub\x.heic"),
            (Some("sub".to_string()), Some("x.heic".to_string()))
        );
    }

    // --- datetime salience + junk filtering ----------------------------------------

    #[test]
    fn datetime_is_normalized_from_exif_colons() {
        let e = ex(&[("DateTimeOriginal", "2023:05:01 14:30:07")]);
        assert_eq!(
            salient_datetime(&e, Some((2026, 7, 4))),
            Some("2023-05-01 14:30:07".to_string())
        );
    }

    #[test]
    fn datetime_falls_back_to_plain_datetime_tag() {
        let e = ex(&[("DateTime", "2019:12:25 09:00:00")]);
        assert_eq!(
            salient_datetime(&e, None),
            Some("2019-12-25 09:00:00".to_string())
        );
    }

    #[test]
    fn datetime_drops_epoch_default_stamps() {
        for junk in [
            "1970:01:01 00:00:00",
            "1980:01:01 00:00:00",
            "2000:01:01 00:00:00",
        ] {
            let e = ex(&[("DateTimeOriginal", junk)]);
            assert_eq!(salient_datetime(&e, None), None, "should drop {junk}");
        }
        // 2000-01-01 at a real time is a legitimate photo, not the epoch default.
        let e = ex(&[("DateTimeOriginal", "2000:01:01 12:34:56")]);
        assert!(salient_datetime(&e, None).is_some());
    }

    #[test]
    fn datetime_drops_future_dates_relative_to_now() {
        let e = ex(&[("DateTimeOriginal", "2030:01:01 00:00:01")]);
        assert_eq!(salient_datetime(&e, Some((2026, 7, 4))), None);
        // Without a "now", the future check is skipped.
        assert!(salient_datetime(&e, None).is_some());
    }

    #[test]
    fn datetime_none_when_missing_or_unparseable() {
        assert_eq!(salient_datetime(&ex(&[]), None), None);
        assert_eq!(
            salient_datetime(&ex(&[("DateTimeOriginal", "not a date")]), None),
            None
        );
        assert_eq!(
            salient_datetime(&ex(&[("DateTimeOriginal", "0000:00:00 00:00:00")]), None),
            None
        );
    }

    // --- camera salience -----------------------------------------------------------

    #[test]
    fn camera_dedupes_make_inside_model_and_appends_lens() {
        let e = ex(&[
            ("Make", "NIKON CORPORATION"),
            ("Model", "NIKON D850"),
            ("LensModel", "24-70mm f/2.8"),
        ]);
        // "NIKON D850" already starts with neither the full make; make+model concatenated,
        // then de-dupe leaves the model when it begins with the make token.
        assert_eq!(
            salient_camera(&e),
            Some("NIKON D850, 24-70mm f/2.8 lens".to_string())
        );
    }

    #[test]
    fn camera_joins_make_and_model_when_distinct() {
        let e = ex(&[("Make", "SONY"), ("Model", "ILCE-7RM4")]);
        assert_eq!(salient_camera(&e), Some("SONY ILCE-7RM4".to_string()));
    }

    #[test]
    fn camera_none_without_make_or_model() {
        assert_eq!(salient_camera(&ex(&[("LensModel", "50mm")])), None);
    }

    // --- location / GPS ------------------------------------------------------------

    #[test]
    fn location_parses_dms_to_signed_decimal() {
        let e = ex(&[
            ("GPSLatitude", "43 deg 39 min 8.9 sec"),
            ("GPSLatitudeRef", "N"),
            ("GPSLongitude", "79 deg 23 min 15.6 sec"),
            ("GPSLongitudeRef", "W"),
        ]);
        // 43 + 39/60 + 8.9/3600 = 43.65247…; west longitude is negative.
        assert_eq!(
            salient_location(&e),
            Some("43.65247, -79.38767".to_string())
        );
    }

    #[test]
    fn location_accepts_already_decimal_values() {
        let e = ex(&[
            ("GPSLatitude", "43.6525"),
            ("GPSLatitudeRef", "N"),
            ("GPSLongitude", "79.3877"),
            ("GPSLongitudeRef", "W"),
        ]);
        assert_eq!(
            salient_location(&e),
            Some("43.65250, -79.38770".to_string())
        );
    }

    #[test]
    fn location_none_without_both_coordinates() {
        assert_eq!(salient_location(&ex(&[("GPSLatitude", "43.6")])), None);
        assert_eq!(salient_location(&ex(&[])), None);
    }

    // --- build_context end to end --------------------------------------------------

    #[test]
    fn build_context_gathers_all_salient_fields() {
        let e = ex(&[
            ("Make", "Apple"),
            ("Model", "iPhone 15 Pro"),
            ("DateTimeOriginal", "2024:06:01 18:05:00"),
            ("GPSLatitude", "48.8584"),
            ("GPSLatitudeRef", "N"),
            ("GPSLongitude", "2.2945"),
            ("GPSLongitudeRef", "E"),
        ]);
        let ctx = build_context("Paris/eiffel.heic", &e, Some((2026, 7, 4)));
        assert_eq!(ctx.filename.as_deref(), Some("eiffel.heic"));
        assert_eq!(ctx.folder.as_deref(), Some("Paris"));
        assert_eq!(ctx.datetime.as_deref(), Some("2024-06-01 18:05:00"));
        assert_eq!(ctx.camera.as_deref(), Some("Apple iPhone 15 Pro"));
        assert_eq!(ctx.location.as_deref(), Some("48.85840, 2.29450"));
        assert!(!ctx.is_empty());
    }

    // --- build_prompt / templating -------------------------------------------------

    #[test]
    fn default_prompt_without_metadata_is_the_bare_instruction() {
        let ctx = DescribeContext::default();
        assert!(ctx.is_empty());
        assert_eq!(build_prompt(&ctx, None), DEFAULT_INSTRUCTION);
    }

    #[test]
    fn default_prompt_appends_context_block_with_preamble() {
        let ctx = DescribeContext {
            filename: Some("cat.jpg".to_string()),
            folder: Some("pets".to_string()),
            ..Default::default()
        };
        let p = build_prompt(&ctx, None);
        assert!(p.starts_with(DEFAULT_INSTRUCTION));
        assert!(p.contains(CONTEXT_PREAMBLE));
        assert!(p.contains("Filename: cat.jpg"));
        assert!(p.contains("Folder: pets"));
        // Only present fields appear.
        assert!(!p.contains("Taken:"));
        assert!(!p.contains("Camera:"));
    }

    #[test]
    fn custom_template_substitutes_individual_placeholders() {
        let ctx = DescribeContext {
            filename: Some("x.jpg".to_string()),
            datetime: Some("2024-01-01 00:00:01".to_string()),
            camera: None,
            ..Default::default()
        };
        let t = "Look at {filename} taken {datetime}. Camera:{camera}.";
        assert_eq!(
            build_prompt(&ctx, Some(t)),
            "Look at x.jpg taken 2024-01-01 00:00:01. Camera:."
        );
    }

    #[test]
    fn custom_template_context_placeholder_expands_the_block() {
        let ctx = DescribeContext {
            filename: Some("x.jpg".to_string()),
            camera: Some("SONY A7".to_string()),
            ..Default::default()
        };
        assert_eq!(
            build_prompt(&ctx, Some("<<{context}>>")),
            "<<Filename: x.jpg\nCamera: SONY A7>>"
        );
    }

    #[test]
    fn context_block_orders_lines_filename_folder_taken_camera_location() {
        let ctx = DescribeContext {
            filename: Some("f".to_string()),
            folder: Some("d".to_string()),
            datetime: Some("t".to_string()),
            camera: Some("c".to_string()),
            location: Some("l".to_string()),
        };
        assert_eq!(
            ctx.block(),
            "Filename: f\nFolder: d\nTaken: t\nCamera: c\nLocation: l"
        );
    }
}
