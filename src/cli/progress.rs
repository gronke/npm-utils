//! Stderr status reporting for long-running verbs, behind the global `-q`/`--quiet`.
//!
//! Status lines are *chatter*, not results: they go to stderr, never stdout (reports stay
//! machine-consumable), never carry the `npm-utils:` error prefix (that marker is reserved for
//! errors and warnings, which `--quiet` never suppresses), and vanish under `--quiet`. A TTY gets
//! a live in-place counter for counted phases; piped/CI stderr gets plain begin/completion lines.
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
}

impl Phase {
    fn begin(sink: Sink, label: String) -> Phase {
        match sink {
            Sink::Off => {}
            Sink::Plain => eprintln!("{}", begin_line(&label)),
            Sink::Tty => eprint!("{}", begin_line(&label)),
        }
        Phase {
            sink,
            label,
            started: Instant::now(),
            open: sink == Sink::Tty,
        }
    }

    /// Update the live counter — counted phases on a TTY; silent elsewhere.
    pub(super) fn tick(&mut self, count: usize) {
        if self.sink == Sink::Tty {
            eprint!("\r{}", tick_line(&self.label, count));
        }
    }

    /// Complete the phase: print `{label} ... {summary} ({secs}s)` and release the terminal line.
    pub(super) fn finish(mut self, summary: &str) {
        let line = finish_line(&self.label, summary, self.started.elapsed().as_secs_f64());
        match self.sink {
            Sink::Off => {}
            Sink::Plain => eprintln!("{line}"),
            Sink::Tty => eprint!("\r{line}\n"),
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

/// `{label} ... {count}` capped to [`TICK_WIDTH`] characters (char-boundary safe) — the transient
/// TTY counter render. The count grows monotonically and the finish line embeds it plus more, so
/// every rewrite is at least as long as the last and no stale characters survive.
fn tick_line(label: &str, count: usize) -> String {
    let line = format!("{label} ... {count}");
    if line.chars().count() > TICK_WIDTH {
        line.chars().take(TICK_WIDTH).collect()
    } else {
        line
    }
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
        assert_eq!(tick_line("resolving", 37), "resolving ... 37");
        // A label pushing the render past the cap — with a multibyte char near the boundary —
        // truncates to exactly TICK_WIDTH chars without splitting a codepoint.
        let wide = format!("{}é", "x".repeat(90));
        let capped = tick_line(&wide, 12345);
        assert_eq!(capped.chars().count(), TICK_WIDTH);
        assert!(capped.starts_with("xxx"));
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
        counted.tick(1);
        counted.tick(2);
        counted.finish("2 packages");
        let step = progress.step("querying npm advisories");
        drop(step); // unfinished — the Drop backstop must be a no-op off-TTY
    }
}
