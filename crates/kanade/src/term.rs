//! Central colour policy and semantic palette for human-facing output.
//!
//! Machine-readable output (JSON / YAML / tokens / manifests / remote command
//! output) never goes through the palette. Only labels, headings, status words
//! and notices do, and only when the stream they are written to supports
//! colour. Padding and column widths are always computed on plain text first;
//! styling wraps a finished cell, so stripping the escapes yields exactly the
//! plain rendering.

use std::ffi::OsStr;
use std::fmt::Display;
use std::io;

use anstream::{AutoStream, ColorChoice};
use clap::ValueEnum;
use clap::builder::styling::{AnsiColor, Effects, Style, Styles};

/// `--color` / `KANADE_COLOR`. Mirrors clap's own colour choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorPolicy {
    /// Colour when the stream is a terminal; honours `NO_COLOR`,
    /// `CLICOLOR` and `CLICOLOR_FORCE`.
    #[default]
    Auto,
    /// Always emit ANSI colour, even when piped.
    Always,
    /// Never emit ANSI colour.
    Never,
}

impl ColorPolicy {
    fn clap(self) -> clap::ColorChoice {
        match self {
            Self::Auto => clap::ColorChoice::Auto,
            Self::Always => clap::ColorChoice::Always,
            Self::Never => clap::ColorChoice::Never,
        }
    }

    fn anstream(self) -> ColorChoice {
        match self {
            Self::Auto => ColorChoice::Auto,
            Self::Always => ColorChoice::Always,
            Self::Never => ColorChoice::Never,
        }
    }
}

/// Apply the policy process-wide (clap help, tracing and every `Palette`).
pub fn init(policy: ColorPolicy) {
    policy.anstream().write_global();
}

/// The clap colour setting for a policy.
pub fn clap_choice(policy: ColorPolicy) -> clap::ColorChoice {
    policy.clap()
}

// Unlike anstream's print macros, these retain std's panic on write errors,
// including BrokenPipe, while still adapting colors to Windows consoles.
macro_rules! human_print {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        write!(anstream::stdout(), $($arg)*).expect("failed printing to stdout")
    }};
}

macro_rules! human_println {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        writeln!(anstream::stdout(), $($arg)*).expect("failed printing to stdout")
    }};
}

macro_rules! human_eprintln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        writeln!(anstream::stderr(), $($arg)*).expect("failed printing to stderr")
    }};
}

pub(crate) use human_eprintln as eprintln;
pub(crate) use human_print as print;
pub(crate) use human_println as println;

/// Find the colour policy in raw arguments before clap parses them, so that
/// `--help` and usage errors honour it. Stops at `--` (everything after is
/// positional). Falls back to `KANADE_COLOR`, then `auto`. Unparseable values
/// are ignored here; clap reports them during the real parse.
pub fn prescan<I, S>(args: I, env: Option<&str>) -> ColorPolicy
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let parse = |v: &str| ColorPolicy::from_str(v, true).ok();
    let mut found = None;
    let mut it = args.into_iter().skip(1);
    while let Some(a) = it.next() {
        let Some(a) = a.as_ref().to_str() else {
            continue;
        };
        if a == "--" {
            break;
        }
        if let Some(v) = a.strip_prefix("--color=") {
            found = parse(v).or(found);
        } else if a == "--color"
            && let Some(v) = it.next()
        {
            found = v.as_ref().to_str().and_then(parse).or(found);
        }
    }
    found.or_else(|| env.and_then(parse)).unwrap_or_default()
}

/// Help / usage styling built from the same hues as [`Palette`]. Headings are
/// bold in the terminal's default colour; no fixed backgrounds or greys.
pub const fn clap_styles() -> Styles {
    Styles::styled()
        .header(Style::new().effects(Effects::BOLD))
        .usage(Style::new().effects(Effects::BOLD))
        .literal(Style::new().fg_color(Some(anstyle_color(AnsiColor::Cyan))))
        .placeholder(Style::new())
        .error(
            Style::new()
                .fg_color(Some(anstyle_color(AnsiColor::Red)))
                .effects(Effects::BOLD),
        )
        .valid(Style::new().fg_color(Some(anstyle_color(AnsiColor::Green))))
        .invalid(Style::new().fg_color(Some(anstyle_color(AnsiColor::Yellow))))
}

const fn anstyle_color(c: AnsiColor) -> clap::builder::styling::Color {
    clap::builder::styling::Color::Ansi(c)
}

/// Semantic styles for one output stream.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    on: bool,
}

impl Palette {
    /// No styling at all (tests).
    #[cfg(test)]
    pub const fn plain() -> Self {
        Self { on: false }
    }

    /// Always styled, regardless of environment (tests).
    #[cfg(test)]
    pub const fn colored() -> Self {
        Self { on: true }
    }

    fn for_stream<S: anstream::stream::RawStream>(s: &S) -> Self {
        Self {
            on: AutoStream::choice(s) != ColorChoice::Never,
        }
    }

    /// Palette for stdout under the active policy.
    pub fn stdout() -> Self {
        Self::for_stream(&io::stdout())
    }

    /// Palette for stderr under the active policy.
    pub fn stderr() -> Self {
        Self::for_stream(&io::stderr())
    }

    /// Whether this palette emits escapes (tracing needs the bool).
    pub fn enabled(&self) -> bool {
        self.on
    }

    fn paint(&self, style: Style, s: impl Display) -> String {
        if self.on {
            format!("{}{s}{}", style.render(), style.render_reset())
        } else {
            s.to_string()
        }
    }

    /// Section titles and table headers: bold, default colour.
    pub fn heading(&self, s: impl Display) -> String {
        self.paint(Style::new().effects(Effects::BOLD), s)
    }

    /// Field labels / keys: cyan.
    pub fn label(&self, s: impl Display) -> String {
        self.paint(
            Style::new().fg_color(Some(anstyle_color(AnsiColor::Cyan))),
            s,
        )
    }

    /// Success: green.
    pub fn ok(&self, s: impl Display) -> String {
        self.paint(
            Style::new().fg_color(Some(anstyle_color(AnsiColor::Green))),
            s,
        )
    }

    /// Warning: yellow.
    pub fn warn(&self, s: impl Display) -> String {
        self.paint(
            Style::new().fg_color(Some(anstyle_color(AnsiColor::Yellow))),
            s,
        )
    }

    /// Failure: bold red.
    pub fn err(&self, s: impl Display) -> String {
        self.paint(
            Style::new()
                .fg_color(Some(anstyle_color(AnsiColor::Red)))
                .effects(Effects::BOLD),
            s,
        )
    }

    /// Secondary decoration only (table rules, hints): dim.
    pub fn dim(&self, s: impl Display) -> String {
        self.paint(Style::new().effects(Effects::DIMMED), s)
    }
}

/// Remove CSI escape sequences (test helper for plain-equals-stripped checks).
/// `✓` for stdout success lines.
pub fn ok_mark() -> String {
    Palette::stdout().ok("✓")
}

/// `⚠` for stdout warning lines.
pub fn warn_mark() -> String {
    Palette::stdout().warn("⚠")
}

/// `✗` for stderr failure lines.
pub fn fail_mark() -> String {
    Palette::stderr().err("✗")
}

/// Remove CSI escape sequences (test helper).
#[cfg(test)]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prescan_precedence_and_forms() {
        let a = |v: &[&str]| prescan(v.iter().copied(), None);
        assert_eq!(a(&["k"]), ColorPolicy::Auto);
        assert_eq!(a(&["k", "--color=never"]), ColorPolicy::Never);
        assert_eq!(a(&["k", "--color", "always", "job"]), ColorPolicy::Always);
        // after `--` is positional data
        assert_eq!(a(&["k", "--", "--color=always"]), ColorPolicy::Auto);
        // command line beats environment, environment beats default
        assert_eq!(
            prescan(["k", "--color=never"], Some("always")),
            ColorPolicy::Never
        );
        assert_eq!(prescan(["k"], Some("always")), ColorPolicy::Always);
        // invalid values fall through to clap's own error
        assert_eq!(
            prescan(["k", "--color=x"], Some("never")),
            ColorPolicy::Never
        );
    }

    #[test]
    fn palette_plain_is_identity_and_colored_strips_back() {
        let p = Palette::plain();
        assert_eq!(p.ok("done"), "done");
        let c = Palette::colored();
        for s in [
            c.heading("h"),
            c.label("h"),
            c.ok("h"),
            c.warn("h"),
            c.err("h"),
            c.dim("h"),
        ] {
            assert!(s.contains('\x1b'), "{s:?}");
            assert_eq!(strip_ansi(&s), "h");
        }
    }
}
