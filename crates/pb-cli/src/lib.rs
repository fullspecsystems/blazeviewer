//! PhotoBlaze CLI surface (task #78): the clap argument parser and the pure mapping
//! into [`pb_app_core::LaunchOverrides`].
//!
//! This is a **library**, not part of the `pb-app` binary, so the parser is shareable:
//! `pb-app` (the winit shell) links it today; the macOS FFI host (`pb-mac-ffi`) can link
//! the *same* parser later and feed Swift's `CommandLine.arguments` through
//! [`parse_from`]. One source of truth for the flag surface, across shells.
//!
//! Two deliberate design choices keep this FFI-safe and shell-agnostic:
//! - **No `process::exit`.** [`parse_from`] returns `Result<Cli, clap::Error>`; a
//!   `--help` / `--version` request or a parse error comes back as a [`clap::Error`]
//!   whose `.kind()` / `.exit_code()` / `.render()` each shell handles itself (print +
//!   exit on Windows/Linux; a dialog or a no-op on the macOS FFI). A library must never
//!   kill its host process.
//! - **argv is always a parameter** (never `std::env` inside the lib), so every shell
//!   feeds its own argument source.
//!
//! Only the *result* ([`LaunchOverrides`]) lives in `pb-app-core`; parsing is a
//! shell/entrypoint concern, and the core stays clap-free (NS0, toml-only).

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::styling::AnsiColor;
use clap::builder::Styles;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use pb_app_core::launch::{LaunchOverrides, SlideshowStart, StartAt};
use pb_app_core::settings::{AppearanceMode, ScaleModePref};
use pb_app_core::slideshow;

/// Re-exported so the shells share exactly one clap version and can match on
/// `pb_cli::clap::error::ErrorKind` without declaring clap themselves.
pub use clap;

const AFTER_HELP: &str = "\
EXAMPLES:
  photoblaze ~/Photos
  photoblaze ~/Photos --slideshow=3s --shuffle
  photoblaze album.zip --fullscreen --scale fill
  photoblaze ~/Photos --reverse --start-at 100";

/// Help colouring for terminals that support it. clap's `color` feature auto-detects a TTY and
/// falls back to plain text when piped / redirected, so this never garbles captured output. One
/// place to retune the palette: bold headers, cyan flags, green value placeholders.
const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightYellow.on_default().bold())
    .usage(AnsiColor::BrightYellow.on_default().bold())
    .literal(AnsiColor::BrightCyan.on_default())
    .placeholder(AnsiColor::BrightGreen.on_default());

/// The PhotoBlaze command line. Every flag maps to existing viewer state via
/// [`Cli::to_overrides`]; nothing here persists to `settings.toml`.
#[derive(Parser, Debug)]
#[command(
    name = "photoblaze",
    bin_name = "photoblaze",
    // A placeholder version so clap wires up the `--version` flag; the shell overrides
    // the string with the real build id (same as the About box) in `parse_from`.
    version,
    // One tagline for the whole app (About box, file association, here). See
    // `pb_app_core::TAGLINE`.
    about = pb_app_core::TAGLINE,
    after_help = AFTER_HELP,
    styles = HELP_STYLES,
    max_term_width = 100
)]
pub struct Cli {
    /// Image files, a folder, or an archive (.zip / .7z) to open. With no path,
    /// PhotoBlaze opens empty.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Start in a window (this launch only).
    #[arg(short = 'w', long, overrides_with = "fullscreen")]
    pub windowed: bool,
    /// Start borderless-fullscreen (this launch only).
    #[arg(short = 'f', long, overrides_with = "windowed")]
    pub fullscreen: bool,

    /// Open folders recursively (include subfolders).
    #[arg(short = 'r', long, overrides_with = "no_recursive")]
    pub recursive: bool,
    /// Open the folder flat (top level only).
    #[arg(long = "no-recursive", overrides_with = "recursive")]
    pub no_recursive: bool,

    /// Show the info line on launch.
    #[arg(long, overrides_with = "no_info")]
    pub info: bool,
    /// Hide the info line on launch.
    #[arg(long = "no-info", overrides_with = "info")]
    pub no_info: bool,

    /// Open the image details (Inspector) panel on launch.
    #[arg(long)]
    pub details: bool,
    /// Open the folder tree panel on launch.
    #[arg(long)]
    pub folders: bool,

    /// Start a slideshow, optionally at a per-slide interval: seconds by default,
    /// or with a unit suffix (e.g. --slideshow, --slideshow=5, --slideshow=3s,
    /// --slideshow=0.5m). Out-of-range values are clamped to 0.1..60 seconds.
    #[arg(
        long,
        value_name = "SECS",
        num_args = 0..=1,
        require_equals = true,
        value_parser = parse_interval
    )]
    pub slideshow: Option<Option<f64>>,

    /// Navigate in the precomputed random (shuffle) order.
    #[arg(long)]
    pub shuffle: bool,
    /// Play backward. Combine with --shuffle for reverse shuffle.
    #[arg(long)]
    pub reverse: bool,

    /// Initial scale mode.
    #[arg(long, value_name = "fit|fill|original", hide_possible_values = true)]
    pub scale: Option<ScaleArg>,
    /// Light / dark theme for this launch.
    #[arg(long, value_name = "light|dark|system", hide_possible_values = true)]
    pub theme: Option<ThemeArg>,

    /// Mute Live Photo audio.
    #[arg(long)]
    pub mute: bool,

    /// Open at photo N (1-based), or the first file whose name matches.
    #[arg(long, value_name = "N|NAME")]
    pub start_at: Option<String>,

    /// Force a standalone instance: skip the Windows single-instance election so this
    /// launch opens its own window instead of forwarding to a running one (task #14).
    #[arg(long, hide = true)]
    pub new_window: bool,

    /// Hidden macOS back-compat: the pre-CLI launch flag (`open … --args --pb-open
    /// <path>`), kept working because a bare path in `argv[1]` triggers AppKit's
    /// document-open machinery (the windowless-WindowGroup gotcha) — a `--`-prefixed
    /// form dodges it. Folded into the positional paths by [`Cli::launch_paths`].
    #[arg(long = "pb-open", value_name = "PATH", hide = true)]
    pub pb_open: Option<PathBuf>,

    /// Print per-stage decode/timing report on exit (dev).
    #[arg(long, hide = true)]
    pub metrics: bool,
}

/// `--scale` values, mirroring [`ScaleModePref`] so the core stays clap-free.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScaleArg {
    Fit,
    Fill,
    Original,
}

/// `--theme` values, mirroring [`AppearanceMode`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ThemeArg {
    Light,
    Dark,
    System,
}

impl Cli {
    /// Every path the command line asked to open: the positionals plus the hidden
    /// `--pb-open` back-compat alias. Shells consume this, not `paths`, so the alias
    /// can never be silently dropped.
    pub fn launch_paths(&self) -> Vec<PathBuf> {
        let mut all = self.paths.clone();
        all.extend(self.pb_open.clone());
        all
    }

    /// Map the parsed command line into the session-only [`LaunchOverrides`] the core
    /// consumes. Pure — no I/O, no `std::env`, no exit.
    pub fn to_overrides(&self) -> LaunchOverrides {
        LaunchOverrides {
            // `overrides_with` guarantees at most one of each pair is set (last-one-wins
            // on the command line), so `tri` never sees a genuine conflict.
            windowed: tri(self.windowed, self.fullscreen),
            recursive: tri(self.recursive, self.no_recursive),
            show_info: tri(self.info, self.no_info),
            scale: self.scale.map(|s| match s {
                ScaleArg::Fit => ScaleModePref::Fit,
                ScaleArg::Fill => ScaleModePref::Fill,
                ScaleArg::Original => ScaleModePref::Original,
            }),
            theme: self.theme.map(|t| match t {
                ThemeArg::Light => AppearanceMode::Light,
                ThemeArg::Dark => AppearanceMode::Dark,
                ThemeArg::System => AppearanceMode::System,
            }),
            mute: self.mute.then_some(true),
            open_details: self.details,
            open_folders: self.folders,
            slideshow: self.slideshow.map(|secs| SlideshowStart {
                interval_secs: secs.map(clamp_interval_secs),
            }),
            shuffle: self.shuffle,
            reverse: self.reverse,
            start_at: self.start_at.as_deref().map(parse_start_at),
            new_window: self.new_window,
            metrics: self.metrics,
        }
    }
}

/// Parse `args` (any argv source, e.g. `std::env::args_os()` or Swift's
/// `CommandLine.arguments`) with `version` as the `--version` / help build string.
/// **Never exits** — `--help` / `--version` / bad input all come back as
/// `Err(clap::Error)`; the caller renders it (`.render()`) and picks an exit
/// (`.exit_code()`: 0 for help/version, 2 for a usage error).
pub fn parse_from<I, T>(args: I, version: &str) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // clap's `Str` only accepts `&'static str`; the version string lives for the whole
    // run, so leaking one small copy at startup is fine and keeps a `'static` bound off
    // every caller.
    let version: &'static str = Box::leak(version.to_string().into_boxed_str());
    let cmd = Cli::command()
        .version(version)
        // The ripgrep convention (binary `rg`, version line "ripgrep X.Y"): usage lines
        // keep the lowercase COMMAND the user types (`photoblaze`, the bin/repo name);
        // the PRODUCT-name contexts wear the brand — `--version` prints
        // "PhotoBlaze <version>" via display_name, and the help header (about) below.
        .display_name("PhotoBlaze")
        .about(BRANDED_ABOUT.as_str());
    let matches = cmd.try_get_matches_from(args)?;
    Cli::from_arg_matches(&matches)
}

/// The branded help header: the product name + the shared tagline ("PhotoBlaze — an
/// ultra-fast, capable image viewer"). Composed from [`pb_app_core::TAGLINE`] at first
/// use so the one-tagline rule holds (About box, file association, and here can never
/// drift); the first letter is lowercased to read naturally after the em dash.
static BRANDED_ABOUT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let t = pb_app_core::TAGLINE;
    let mut c = t.chars();
    match c.next() {
        Some(first) => format!("PhotoBlaze — {}{}", first.to_lowercase(), c.as_str()),
        None => "PhotoBlaze".to_string(),
    }
});

/// Resolve a positive/negative flag pair into a tri-state override. Positive wins if
/// both are somehow set (shouldn't happen under `overrides_with`).
fn tri(positive: bool, negative: bool) -> Option<bool> {
    if positive {
        Some(true)
    } else if negative {
        Some(false)
    } else {
        None
    }
}

/// Parse a `--slideshow` interval value into seconds: a bare number is seconds
/// (back-compat: `--slideshow=5`), a trailing `s` / `m` (case-insensitive) is an
/// explicit seconds / minutes unit (`3s`, `0.5m`). Anything else — other units,
/// internal whitespace (`3 s`), garbage — is a clap value error, not a clamp. The
/// numeric value itself is clamped downstream ([`clamp_interval_secs`]), so `5m`
/// parses to 300 s and then clamps to the 60 s ceiling (Mixed strictness).
fn parse_interval(s: &str) -> Result<f64, String> {
    let t = s.trim();
    let (num, mult) = match t.to_ascii_lowercase().as_str() {
        u if u.ends_with('s') => (&t[..t.len() - 1], 1.0),
        u if u.ends_with('m') => (&t[..t.len() - 1], 60.0),
        _ => (t, 1.0),
    };
    // f64::from_str rejects embedded/trailing whitespace, so "3 s" fails here.
    num.parse::<f64>()
        .map(|n| n * mult)
        .map_err(|_| format!("not a duration: '{s}' (seconds, or a unit: 3s, 0.5m)"))
}

/// Clamp a `--slideshow=SECS` value into `[MIN_INTERVAL, MAX_INTERVAL]` (Mixed
/// strictness: out-of-range clamps, it does not error). Guards NaN / negative before
/// they could reach `Duration::from_secs_f64` (which panics on those).
fn clamp_interval_secs(secs: f64) -> f64 {
    let min = slideshow::MIN_INTERVAL.as_secs_f64();
    let max = slideshow::MAX_INTERVAL.as_secs_f64();
    if secs.is_nan() {
        min
    } else {
        secs.clamp(min, max)
    }
}

/// `--start-at`: an all-digits value is a 1-based index (clamped to the deck at resolve
/// time); anything else is a file-name match.
fn parse_start_at(s: &str) -> StartAt {
    let t = s.trim();
    match t.parse::<usize>() {
        Ok(n) => StartAt::Index(n),
        Err(_) => StartAt::Name(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_app_core::Nav;

    /// Parse just the flags (no runtime version needed) via the derive entry.
    fn parse(args: &[&str]) -> Cli {
        let mut full = vec!["photoblaze"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).expect("should parse")
    }

    fn overrides(args: &[&str]) -> LaunchOverrides {
        parse(args).to_overrides()
    }

    #[test]
    fn no_args_is_the_default_override() {
        assert_eq!(overrides(&[]), LaunchOverrides::default());
    }

    #[test]
    fn positional_paths_are_collected() {
        let cli = parse(&["a.jpg", "b.png", "album.zip"]);
        assert_eq!(
            cli.paths,
            vec![
                PathBuf::from("a.jpg"),
                PathBuf::from("b.png"),
                PathBuf::from("album.zip")
            ]
        );
    }

    #[test]
    fn window_mode_is_last_one_wins() {
        assert_eq!(overrides(&["-w"]).windowed, Some(true));
        assert_eq!(overrides(&["-f"]).windowed, Some(false));
        assert_eq!(overrides(&["--windowed"]).windowed, Some(true));
        assert_eq!(overrides(&["--fullscreen"]).windowed, Some(false));
        // Both given: the last on the line wins (Mixed strictness, no conflict error).
        assert_eq!(overrides(&["-f", "-w"]).windowed, Some(true));
        assert_eq!(overrides(&["-w", "-f"]).windowed, Some(false));
    }

    #[test]
    fn recursive_and_info_are_tri_state() {
        assert_eq!(overrides(&[]).recursive, None);
        assert_eq!(overrides(&["-r"]).recursive, Some(true));
        assert_eq!(overrides(&["--no-recursive"]).recursive, Some(false));
        assert_eq!(overrides(&["--no-recursive", "-r"]).recursive, Some(true));

        assert_eq!(overrides(&[]).show_info, None);
        assert_eq!(overrides(&["--info"]).show_info, Some(true));
        assert_eq!(overrides(&["--no-info"]).show_info, Some(false));
    }

    #[test]
    fn panel_flags_are_one_way_opens() {
        let o = overrides(&["--details", "--folders"]);
        assert!(o.open_details);
        assert!(o.open_folders);
        let none = overrides(&[]);
        assert!(!none.open_details);
        assert!(!none.open_folders);
    }

    #[test]
    fn slideshow_optional_value_and_clamping() {
        // Absent.
        assert_eq!(overrides(&[]).slideshow, None);
        // Present, no value -> start at the saved/default interval.
        assert_eq!(
            overrides(&["--slideshow"]).slideshow,
            Some(SlideshowStart {
                interval_secs: None
            })
        );
        // Present with a value.
        assert_eq!(
            overrides(&["--slideshow=5"]).slideshow,
            Some(SlideshowStart {
                interval_secs: Some(5.0)
            })
        );
        // Above max -> clamped to 60; below min / negative / zero -> clamped to 0.1.
        assert_eq!(
            overrides(&["--slideshow=999"]).slideshow,
            Some(SlideshowStart {
                interval_secs: Some(60.0)
            })
        );
        assert_eq!(
            overrides(&["--slideshow=0"]).slideshow,
            Some(SlideshowStart {
                interval_secs: Some(0.1)
            })
        );
        // Negative must not panic (Duration::from_secs_f64 would) — clamps to min.
        assert_eq!(
            overrides(&["--slideshow=-5"]).slideshow,
            Some(SlideshowStart {
                interval_secs: Some(0.1)
            })
        );
    }

    #[test]
    fn slideshow_unit_suffixes() {
        let secs = |args: &[&str]| overrides(args).slideshow.unwrap().interval_secs;
        // Explicit seconds, fractional minutes, case-insensitive.
        assert_eq!(secs(&["--slideshow=3s"]), Some(3.0));
        assert_eq!(secs(&["--slideshow=0.5m"]), Some(30.0));
        assert_eq!(secs(&["--slideshow=1m"]), Some(60.0));
        assert_eq!(secs(&["--slideshow=3S"]), Some(3.0));
        // Minutes above the ceiling clamp (Mixed strictness), same as bare seconds.
        assert_eq!(secs(&["--slideshow=5m"]), Some(60.0));
        // Unknown units / internal whitespace / bare unit are parse errors.
        assert!(Cli::try_parse_from(["photoblaze", "--slideshow=3h"]).is_err());
        assert!(Cli::try_parse_from(["photoblaze", "--slideshow=3 s"]).is_err());
        assert!(Cli::try_parse_from(["photoblaze", "--slideshow=m"]).is_err());
        // "100ms" strips the trailing s and fails on "100m" — not silently milliseconds.
        assert!(Cli::try_parse_from(["photoblaze", "--slideshow=100ms"]).is_err());
    }

    #[test]
    fn shuffle_and_reverse_drive_the_launch_direction() {
        assert_eq!(overrides(&[]).launch_nav(), Nav::Forward);
        assert_eq!(overrides(&["--reverse"]).launch_nav(), Nav::Backward);
        assert_eq!(overrides(&["--shuffle"]).launch_nav(), Nav::Random);
        assert_eq!(
            overrides(&["--shuffle", "--reverse"]).launch_nav(),
            Nav::RandomPrev
        );
    }

    #[test]
    fn scale_and_theme_map_to_core_enums() {
        assert_eq!(
            overrides(&["--scale", "fill"]).scale,
            Some(ScaleModePref::Fill)
        );
        assert_eq!(
            overrides(&["--scale", "original"]).scale,
            Some(ScaleModePref::Original)
        );
        assert_eq!(
            overrides(&["--theme", "dark"]).theme,
            Some(AppearanceMode::Dark)
        );
        assert_eq!(
            overrides(&["--theme", "system"]).theme,
            Some(AppearanceMode::System)
        );
    }

    #[test]
    fn mute_start_at_new_window_metrics() {
        assert_eq!(overrides(&["--mute"]).mute, Some(true));
        assert_eq!(
            overrides(&["--start-at", "100"]).start_at,
            Some(StartAt::Index(100))
        );
        assert_eq!(
            overrides(&["--start-at", "sunset.jpg"]).start_at,
            Some(StartAt::Name("sunset.jpg".to_string()))
        );
        assert!(overrides(&["--new-window"]).new_window);
        assert!(overrides(&["--metrics"]).metrics);
    }

    #[test]
    fn pb_open_is_a_hidden_path_alias() {
        // The macOS back-compat flag lands in launch_paths() after the positionals.
        let cli = parse(&["a.jpg", "--pb-open", "/photos"]);
        assert_eq!(
            cli.launch_paths(),
            vec![PathBuf::from("a.jpg"), PathBuf::from("/photos")]
        );
        // Alone, it is the whole path set; without it, positionals pass through.
        assert_eq!(
            parse(&["--pb-open", "/photos"]).launch_paths(),
            vec![PathBuf::from("/photos")]
        );
        assert_eq!(
            parse(&["a.jpg"]).launch_paths(),
            vec![PathBuf::from("a.jpg")]
        );
    }

    #[test]
    fn unknown_flag_and_bad_enum_are_errors() {
        assert!(Cli::try_parse_from(["photoblaze", "--nope"]).is_err());
        assert!(Cli::try_parse_from(["photoblaze", "--scale", "huge"]).is_err());
        // A bad interval value (not a number) is a parse error, not a clamp.
        assert!(Cli::try_parse_from(["photoblaze", "--slideshow=abc"]).is_err());
    }

    #[test]
    fn parse_from_never_exits_help_and_version_are_errors() {
        use clap::error::ErrorKind;
        let help = parse_from(["photoblaze", "--help"], "test-build").unwrap_err();
        assert_eq!(help.kind(), ErrorKind::DisplayHelp);
        assert_eq!(help.exit_code(), 0);

        let ver = parse_from(["photoblaze", "--version"], "test-build").unwrap_err();
        assert_eq!(ver.kind(), ErrorKind::DisplayVersion);
        assert_eq!(ver.exit_code(), 0);
        // The shell-supplied build string is what --version prints.
        assert!(ver.to_string().contains("test-build"));

        // A usage error exits 2.
        let bad = parse_from(["photoblaze", "--nope"], "test-build").unwrap_err();
        assert_eq!(bad.exit_code(), 2);
    }
}
