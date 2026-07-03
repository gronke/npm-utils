//! Stderr status reporting for long-running verbs, behind the global `-q`/`--quiet`.
//!
//! Status lines are *chatter*, not results: they go to stderr, never stdout (reports stay
//! machine-consumable), never carry the `npm-utils:` error prefix (that marker is reserved for
//! errors and warnings, which `--quiet` never suppresses), and vanish under `--quiet`. A TTY gets
//! a live in-place line for counted phases; piped/CI stderr gets plain begin/completion lines.
//! Rust's `Stderr` is unbuffered, so every `eprint!` reaches the terminal immediately — no
//! flushing needed.

use std::io::IsTerminal;
use std::time::Instant;

/// Where a phase's lines go: nowhere (`--quiet`), plain terminated lines (piped stderr, and all
/// [`Progress::step`] phases), or a live `\r`-rewritten TTY line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    Off,
    Plain,
    Tty,
}

/// The stderr status reporter — constructed once per invocation from the `--quiet` flag; hands
/// out [`Phase`]s. Every method is a no-op when quiet.
pub(super) struct Progress {
    quiet: bool,
    tty: bool,
}

impl Progress {
    /// Capture `--quiet` and whether stderr is a terminal.
    pub(super) fn new(quiet: bool) -> Progress {
        Progress {
            quiet,
            tty: std::io::stderr().is_terminal(),
        }
    }

    /// Begin a phase whose progress arrives via [`Phase::tick`] (the resolution walk). On a TTY
    /// the line is rewritten in place until [`Phase::finish`] completes it; elsewhere a begin
    /// line prints now and a completion line at finish.
    pub(super) fn counted(&self, label: impl Into<String>) -> Phase {
        let sink = match (self.quiet, self.tty) {
            (true, _) => Sink::Off,
            (false, true) => Sink::Tty,
            (false, false) => Sink::Plain,
        };
        Phase::begin(sink, label.into())
    }

    /// Begin a phase reported as two terminated lines, begin and completion (the advisory-source
    /// queries). It never holds an unterminated line — safe to interleave with warnings the
    /// library emits mid-phase (download retries, OSV record hydration), even on a TTY.
    pub(super) fn step(&self, label: impl Into<String>) -> Phase {
        let sink = if self.quiet { Sink::Off } else { Sink::Plain };
        Phase::begin(sink, label.into())
    }
}

/// One in-flight phase: created by [`Progress::counted`] / [`Progress::step`], ended by
/// [`Phase::finish`]. Dropping an unfinished phase (an error propagating mid-phase) terminates an
/// open TTY line with a bare newline, so the `npm-utils:` error that follows starts at column 0.
pub(super) struct Phase {
    sink: Sink,
    label: String,
    started: Instant,
    /// Whether a TTY line is open (printed without a trailing newline).
    open: bool,
    /// Width of the previous TTY render — a `\r` rewrite only overwrites what it prints, so a
    /// shorter render pads up to this with spaces ([`pad_to_previous`]).
    last_width: usize,
}

impl Phase {
    fn begin(sink: Sink, label: String) -> Phase {
        let opening = begin_line(&label);
        match sink {
            Sink::Off => {}
            Sink::Plain => eprintln!("{opening}"),
            Sink::Tty => eprint!("{opening}"),
        }
        Phase {
            sink,
            started: Instant::now(),
            open: sink == Sink::Tty,
            // Clamped so a pathological >TICK_WIDTH label can never force an over-wide tick pad.
            last_width: match sink {
                Sink::Tty => opening.chars().count().min(TICK_WIDTH),
                _ => 0,
            },
            label,
        }
    }

    /// Update the live line with `detail` (e.g. `"245 lit-html@3.3.3"`) — counted phases on a
    /// TTY; silent elsewhere.
    pub(super) fn tick(&mut self, detail: &str) {
        if self.sink == Sink::Tty {
            let line = tick_line(&self.label, detail);
            eprint!("\r{}", pad_to_previous(&line, &mut self.last_width));
        }
    }

    /// Complete the phase: print `{label} ... {summary} ({secs}s)` and release the terminal line.
    pub(super) fn finish(mut self, summary: &str) {
        let line = finish_line(&self.label, summary, self.started.elapsed().as_secs_f64());
        match self.sink {
            Sink::Off => {}
            Sink::Plain => eprintln!("{line}"),
            Sink::Tty => eprint!("\r{}\n", pad_to_previous(&line, &mut self.last_width)),
        }
        self.open = false;
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if self.open {
            eprintln!();
        }
    }
}

/// Max width of a transient TTY tick render — a `\r` rewrite of a line wider than the terminal
/// would land on the wrong row; 80 columns is the conservative floor. The terminated begin and
/// finish lines are uncapped (they may wrap, harmlessly).
const TICK_WIDTH: usize = 80;

/// `{label} ...` — the phase-begin text.
fn begin_line(label: &str) -> String {
    format!("{label} ...")
}

/// `{label} ... {summary} ({secs:.1}s)` — the completion text. Repeating the label keeps piped
/// log lines self-contained and greppable.
fn finish_line(label: &str, summary: &str, secs: f64) -> String {
    format!("{label} ... {summary} ({secs:.1}s)")
}

/// `{label} ... {detail}` capped to [`TICK_WIDTH`] characters (char-boundary safe) — the
/// transient TTY render. Renders shrink and grow as package names change, so the rewrite pads
/// against the previous width ([`pad_to_previous`]) rather than relying on monotonic length.
fn tick_line(label: &str, detail: &str) -> String {
    let line = format!("{label} ... {detail}");
    if line.chars().count() > TICK_WIDTH {
        line.chars().take(TICK_WIDTH).collect()
    } else {
        line
    }
}

/// Pad `line` with trailing spaces up to the previous render's width — a `\r` rewrite only
/// overwrites what it prints, so a shorter render would leave the old line's tail on screen —
/// then record the new *unpadded* width (anything beyond it is already spaces from this pad).
/// Tick renders are capped at [`TICK_WIDTH`] and the tracker starts clamped to it, so a padded
/// tick never exceeds [`TICK_WIDTH`]; the newline-terminated finish line may, wrapping once,
/// harmlessly.
fn pad_to_previous(line: &str, last_width: &mut usize) -> String {
    let width = line.chars().count();
    let padded = if width < *last_width {
        format!("{line}{}", " ".repeat(*last_width - width))
    } else {
        line.to_string()
    };
    *last_width = width;
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_line_formats_summary_and_elapsed() {
        assert_eq!(
            finish_line("querying npm advisories", "169 advisories", 1.23),
            "querying npm advisories ... 169 advisories (1.2s)"
        );
        assert_eq!(
            begin_line("resolving dependency tree from registry.npmjs.org"),
            "resolving dependency tree from registry.npmjs.org ..."
        );
    }

    #[test]
    fn tick_line_caps_width_on_a_char_boundary() {
        assert_eq!(
            tick_line("resolving", "37 lodash@4.17.21"),
            "resolving ... 37 lodash@4.17.21"
        );
        // A label pushing the render past the cap — with a multibyte char near the boundary —
        // truncates to exactly TICK_WIDTH chars without splitting a codepoint.
        let wide = format!("{}é", "x".repeat(90));
        let capped = tick_line(&wide, "12345");
        assert_eq!(capped.chars().count(), TICK_WIDTH);
        assert!(capped.starts_with("xxx"));
    }

    #[test]
    fn padding_covers_a_shrinking_rewrite() {
        // A shorter render is padded up to the previous width so no stale tail survives the
        // `\r` rewrite; the tracker then records the *unpadded* width.
        let mut last_width = 33;
        let padded = pad_to_previous(&"x".repeat(20), &mut last_width);
        assert_eq!(padded, format!("{}{}", "x".repeat(20), " ".repeat(13)));
        assert_eq!(last_width, 20);
        // A longer render needs no pad.
        let unpadded = pad_to_previous(&"y".repeat(25), &mut last_width);
        assert_eq!(unpadded, "y".repeat(25));
        assert_eq!(last_width, 25);
    }

    #[test]
    fn quiet_phases_are_inert_including_drop_without_finish() {
        // Quiet: full lifecycle plus a dropped-unfinished phase must not print or panic (cargo
        // captures stderr; this is a state-machine smoke test).
        let progress = Progress {
            quiet: true,
            tty: false,
        };
        let mut counted = progress.counted("resolving");
        counted.tick("1 a@1.0.0");
        counted.tick("2 b@2.0.0");
        counted.finish("2 packages");
        let step = progress.step("querying npm advisories");
        drop(step); // unfinished — the Drop backstop must be a no-op off-TTY
    }
}
