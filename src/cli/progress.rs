//! Stderr progress rendering for long-running verbs: named tasks, drawn per the global
//! `--progress` mode.
//!
//! Status output is *chatter*, not results: it goes to stderr, never stdout (reports stay
//! machine-consumable), and never carries the `npm-utils:` prefix — that marker is reserved for
//! warnings and errors, which print in **every** mode. The CLI routes the library's warnings
//! through [`Progress::install_warn_sink`] so they land above a live region instead of tearing
//! it. The modes:
//!
//! - `auto` (default): a live two-line block per task on a terminal; plain begin/finish line
//!   pairs when stderr is piped.
//! - `on` (`1`/`yes`/`true`): the live rendering even when piped (ANSI into the pipe).
//! - `verbose`: one terminated line per event — begin, each item, finish — for logfiles.
//! - `none` (`0`/`off`/`false`/`no`): nothing; `-q`/`--quiet` is its shorthand.
//!
//! A live task renders as `[name] (x/y) title` over one indented detail line, via a single
//! indicatif `ProgressBar` with a newline template (a third info line — rates, sizes — is
//! deliberately future scope). The block height is fixed and only `{wide_msg}` carries
//! unbounded text: indicatif's row accounting assumes no soft wrap, so the header stays short
//! and the detail line truncates to the terminal width.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::{Args, ValueEnum};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};

/// `--progress` values. The variant is `Off` but surfaces as `none` on the CLI, so it never
/// collides with `Option::None` at call sites; the numeric/boolean spellings are hidden aliases.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub(super) enum ProgressMode {
    /// Live rendering on a terminal, plain line pairs when piped
    Auto,
    /// Live rendering even when stderr is piped
    #[value(alias = "1", alias = "yes", alias = "true")]
    On,
    /// One terminated line per event — logfile-friendly, no control characters
    Verbose,
    /// No status output (npm-utils: warnings and errors still print)
    #[value(
        name = "none",
        alias = "0",
        alias = "off",
        alias = "false",
        alias = "no"
    )]
    Off,
}

/// The global stderr-status flags, flattened into the top-level `Cli` (the `LicenseOpts`
/// pattern).
#[derive(Args)]
pub(super) struct DisplayOptions {
    /// Progress rendering on stderr: auto (live on a terminal), on (live even piped), verbose (a line per event), none
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "MODE",
        default_value = "auto"
    )]
    pub(super) progress: ProgressMode,
    /// Suppress status lines on stderr — shorthand for --progress=none (reports on stdout and npm-utils: messages still print)
    #[arg(short = 'q', long, global = true, conflicts_with = "progress")]
    pub(super) quiet: bool,
}

impl DisplayOptions {
    /// The rendering strategy these flags select against the actual stderr.
    pub(super) fn rendering(&self) -> Rendering {
        resolve_rendering(self.progress, self.quiet, std::io::stderr().is_terminal())
    }
}

/// The resolved rendering strategy — a pure function of the flags and stderr's TTY-ness
/// ([`resolve_rendering`]), so the whole selection matrix is unit-testable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Rendering {
    Live { forced: bool },
    Plain,
    Verbose,
    Off,
}

fn resolve_rendering(mode: ProgressMode, quiet: bool, stderr_is_tty: bool) -> Rendering {
    // `-q` conflicts with an explicit `--progress` when clap sees both on the same side of the
    // verb; split across the verb boundary the global-arg propagation doesn't connect them, so
    // quiet winning unconditionally keeps the shorthand deterministic either way.
    if quiet {
        return Rendering::Off;
    }
    match mode {
        ProgressMode::Auto if stderr_is_tty => Rendering::Live { forced: false },
        ProgressMode::Auto => Rendering::Plain,
        ProgressMode::On => Rendering::Live { forced: true },
        ProgressMode::Verbose => Rendering::Verbose,
        ProgressMode::Off => Rendering::Off,
    }
}

/// The stderr progress reporter — constructed once per invocation from [`DisplayOptions`];
/// hands out named [`TaskStatus`]es and (via [`Progress::install_warn_sink`]) keeps the
/// library's `npm-utils:` warnings from corrupting a live region.
pub(super) struct Progress {
    inner: Arc<Backend>,
}

/// Where task events go. `Text` covers both plain (begin/finish pairs) and verbose (a line per
/// item); its writes go through one Mutex so task lines and worker-thread warnings never tear.
/// The boxed writer is stderr in production and an injectable buffer in unit tests. Failed
/// writes are dropped rather than panicking (a closed stderr shouldn't kill an install —
/// a deliberate softening of `eprintln!`).
enum Backend {
    Live(MultiProgress),
    Text {
        verbose: bool,
        out: Mutex<Box<dyn Write + Send>>,
    },
    Off,
}

impl Progress {
    pub(super) fn new(opts: &DisplayOptions) -> Progress {
        Progress::from_rendering(opts.rendering())
    }

    fn from_rendering(rendering: Rendering) -> Progress {
        let backend = match rendering {
            // MultiProgress::new draws to stderr at indicatif's default refresh rate.
            Rendering::Live { forced: false } => Backend::Live(MultiProgress::new()),
            // A `TermLike` target bypasses indicatif's user-attended auto-hide — that *is* the
            // force — but has no default rate limit, so cap it explicitly.
            Rendering::Live { forced: true } => Backend::Live(MultiProgress::with_draw_target(
                ProgressDrawTarget::term_like_with_hz(Box::new(ForcedStderr), 20),
            )),
            Rendering::Plain => Backend::Text {
                verbose: false,
                out: Mutex::new(Box::new(std::io::stderr())),
            },
            Rendering::Verbose => Backend::Text {
                verbose: true,
                out: Mutex::new(Box::new(std::io::stderr())),
            },
            Rendering::Off => Backend::Off,
        };
        Progress {
            inner: Arc::new(backend),
        }
    }

    /// Start a named task: `name` is the short bracket tag (`resolve`, `install`, an advisory
    /// source), `title` the human phase label. Live mode adds the two-line block immediately;
    /// text modes print the begin line.
    pub(super) fn task(&self, name: &str, title: impl Into<String>) -> TaskStatus {
        let title = title.into();
        let bar = match &*self.inner {
            Backend::Live(mp) => {
                let bar = mp.add(
                    ProgressBar::no_length()
                        .with_style(style_bare(name))
                        .with_prefix(title.clone()),
                );
                // Force the initial draw — the block should appear before the first event.
                bar.tick();
                Some(bar)
            }
            Backend::Text { out, .. } => {
                let _ = writeln!(
                    out.lock().expect("progress sink poisoned"),
                    "{}",
                    begin_line(name, &title)
                );
                None
            }
            Backend::Off => None,
        };
        TaskStatus {
            name: name.to_string(),
            title,
            started: Instant::now(),
            finished: AtomicBool::new(false),
            counts: Mutex::new(Counts {
                pos: 0,
                total: None,
            }),
            inner: Arc::clone(&self.inner),
            bar,
            stage: AtomicU8::new(0),
        }
    }

    /// Route the library's `npm-utils:` warnings through this renderer (process-wide; the first
    /// caller wins). Live mode prints them as permanent lines above the region.
    pub(super) fn install_warn_sink(&self) {
        let inner = Arc::clone(&self.inner);
        crate::warn::set_warn_sink(Box::new(move |msg| inner.warn_msg(msg)));
    }
}

impl Backend {
    fn warn_msg(&self, msg: &str) {
        match self {
            Backend::Live(mp) => {
                let _ = mp.println(format!("npm-utils: {msg}"));
            }
            Backend::Text { out, .. } => {
                let _ = writeln!(
                    out.lock().expect("progress sink poisoned"),
                    "npm-utils: {msg}"
                );
            }
            // Warnings always print — `--progress=none` silences status, never warnings.
            Backend::Off => eprintln!("npm-utils: {msg}"),
        }
    }
}

/// One named background task. Methods take `&self` (interior mutability) and the type is `Sync`
/// so resolver worker threads can tick a borrowed handle; it is deliberately not `Clone` —
/// single owner, threads borrow. Ended by [`TaskStatus::finish`] / [`TaskStatus::fail`];
/// dropping an unfinished task (an error propagating mid-phase) clears a live block without a
/// permanent line, so the `npm-utils:` error that follows starts at column 0 with no residue.
pub(super) struct TaskStatus {
    name: String,
    title: String,
    started: Instant,
    finished: AtomicBool,
    counts: Mutex<Counts>,
    inner: Arc<Backend>,
    /// The live block; `None` in the text/off modes.
    bar: Option<ProgressBar>,
    /// Live header stage: 0 bare, 1 counted `(pos)`, 2 totaled `(pos/len)`. Monotonic — a task
    /// that learns its total never falls back, and a count-less task shows no `(0)` noise.
    stage: AtomicU8,
}

struct Counts {
    pos: u64,
    total: Option<u64>,
}

impl TaskStatus {
    /// Declare the item total, upgrading the header to `(pos/len)`. `{len}` enters the live
    /// template only here, so an unset length is never rendered.
    #[allow(dead_code)] // wired by the install-family tickers in a follow-up commit
    pub(super) fn set_total(&self, total: u64) {
        self.counts.lock().expect("progress state poisoned").total = Some(total);
        if let Some(bar) = &self.bar {
            self.stage.store(2, Ordering::SeqCst);
            bar.set_style(style_totaled(&self.name));
            bar.set_length(total);
            // A style/length change alone doesn't mark the draw state dirty — force the redraw
            // so the `(pos/len)` header appears before the next event.
            bar.tick();
        }
    }

    /// Count one completed item. Live: bump the counter and show the item on the detail line;
    /// verbose: one terminated line per item; plain: counted silently (begin/finish pairs only).
    pub(super) fn inc(&self, item: &str) {
        let (pos, total) = {
            let mut counts = self.counts.lock().expect("progress state poisoned");
            counts.pos += 1;
            (counts.pos, counts.total)
        };
        match &*self.inner {
            Backend::Live(_) => {
                if let Some(bar) = &self.bar {
                    // The first count switches the bare header to `(pos)` — unless a total
                    // already upgraded it to `(pos/len)` (the stage is monotonic).
                    if self
                        .stage
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        bar.set_style(style_counted(&self.name));
                    }
                    bar.set_position(pos);
                    bar.set_message(item.to_string());
                }
            }
            Backend::Text { verbose: true, out } => {
                let _ = writeln!(
                    out.lock().expect("progress sink poisoned"),
                    "{}",
                    item_line(&self.name, pos, total, item)
                );
            }
            Backend::Text { .. } | Backend::Off => {}
        }
    }

    /// Update the live detail line with transient state (in-flight fetches, the current file).
    /// Text modes ignore it — transient state is not an item, and logs get one line per item.
    pub(super) fn detail(&self, text: &str) {
        if let Some(bar) = &self.bar {
            bar.set_message(text.to_string());
        }
    }

    /// Items counted so far.
    #[allow(dead_code)] // wired by the install-family tickers in a follow-up commit
    pub(super) fn count(&self) -> u64 {
        self.counts.lock().expect("progress state poisoned").pos
    }

    /// Complete the task: print `[name] title ... summary (secs)` as a permanent line and, in
    /// live mode, clear the block.
    pub(super) fn finish(self, summary: &str) {
        self.complete(summary);
    }

    /// Semantic alias of [`TaskStatus::finish`] for failure summaries — identical rendering
    /// today, a styling seam for later.
    pub(super) fn fail(self, summary: &str) {
        self.complete(summary);
    }

    fn complete(&self, summary: &str) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let line = finish_line(
            &self.name,
            &self.title,
            summary,
            self.started.elapsed().as_secs_f64(),
        );
        match &*self.inner {
            Backend::Live(mp) => {
                // Order matters: print the permanent line above the still-registered block,
                // then clear the block's rows and release the bar.
                let _ = mp.println(line);
                if let Some(bar) = &self.bar {
                    bar.finish_and_clear();
                    mp.remove(bar);
                }
            }
            Backend::Text { out, .. } => {
                let _ = writeln!(out.lock().expect("progress sink poisoned"), "{line}");
            }
            Backend::Off => {}
        }
    }
}

impl Drop for TaskStatus {
    fn drop(&mut self) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            // Unfinished (an error is propagating): clear a live block, print nothing. Text
            // begin lines are already terminated, so there is nothing to release.
            if let (Some(bar), Backend::Live(mp)) = (&self.bar, &*self.inner) {
                bar.finish_and_clear();
                mp.remove(bar);
            }
        }
    }
}

/// Raw-ANSI stderr for `--progress=on`: a `TermLike` draw target bypasses indicatif's
/// user-attended auto-hide, so a piped consumer still receives the live control sequences.
/// There is no real terminal to measure, so the width is the same conservative 80-column floor
/// the old renderer used.
#[derive(Debug)]
struct ForcedStderr;

const FORCED_WIDTH: u16 = 80;

impl TermLike for ForcedStderr {
    fn width(&self) -> u16 {
        FORCED_WIDTH
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        if n > 0 {
            write!(std::io::stderr(), "\x1b[{n}A")
        } else {
            Ok(())
        }
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        if n > 0 {
            write!(std::io::stderr(), "\x1b[{n}B")
        } else {
            Ok(())
        }
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        if n > 0 {
            write!(std::io::stderr(), "\x1b[{n}C")
        } else {
            Ok(())
        }
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        if n > 0 {
            write!(std::io::stderr(), "\x1b[{n}D")
        } else {
            Ok(())
        }
    }
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        let mut err = std::io::stderr();
        err.write_all(s.as_bytes())?;
        err.write_all(b"\n")
    }
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        std::io::stderr().write_all(s.as_bytes())
    }
    fn clear_line(&self) -> std::io::Result<()> {
        write!(std::io::stderr(), "\r\x1b[2K")
    }
    fn flush(&self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// The live templates a task's header moves through. Exactly one `\n` (a fixed two-line block)
/// and only `{wide_msg}` may carry unbounded text — indicatif's row accounting assumes no soft
/// wrap — so titles stay well under the 80-column floor. Task names are crate literals; the
/// templates are static by construction.
fn style_bare(name: &str) -> ProgressStyle {
    style(&format!("[{name}] {{prefix}}\n  {{wide_msg}}"))
}

fn style_counted(name: &str) -> ProgressStyle {
    style(&format!("[{name}] ({{pos}}) {{prefix}}\n  {{wide_msg}}"))
}

fn style_totaled(name: &str) -> ProgressStyle {
    style(&format!(
        "[{name}] ({{pos}}/{{len}}) {{prefix}}\n  {{wide_msg}}"
    ))
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).expect("static progress template")
}

/// `[{name}] {title} ...` — the begin text (plain/verbose modes).
fn begin_line(name: &str, title: &str) -> String {
    format!("[{name}] {title} ...")
}

/// `[{name}] ({pos}/{total}) {item}` — one verbose item line; `({pos})` while the total is
/// unknown.
fn item_line(name: &str, pos: u64, total: Option<u64>, item: &str) -> String {
    match total {
        Some(total) => format!("[{name}] ({pos}/{total}) {item}"),
        None => format!("[{name}] ({pos}) {item}"),
    }
}

/// `[{name}] {title} ... {summary} ({secs:.1}s)` — the completion text in every mode. Repeating
/// the title keeps piped log lines self-contained and greppable.
fn finish_line(name: &str, title: &str, summary: &str, secs: f64) -> String {
    format!("[{name}] {title} ... {summary} ({secs:.1}s)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::InMemoryTerm;

    impl Progress {
        /// A text backend writing into a shared buffer instead of stderr.
        fn text_capture(verbose: bool) -> (Progress, Arc<Mutex<Vec<u8>>>) {
            struct Capture(Arc<Mutex<Vec<u8>>>);
            impl Write for Capture {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let buf = Arc::new(Mutex::new(Vec::new()));
            let progress = Progress {
                inner: Arc::new(Backend::Text {
                    verbose,
                    out: Mutex::new(Box::new(Capture(Arc::clone(&buf)))),
                }),
            };
            (progress, buf)
        }

        /// A live backend drawing onto an in-memory terminal (no rate limit — every state
        /// change draws immediately, so assertions are deterministic).
        fn live_on(term: InMemoryTerm) -> Progress {
            Progress {
                inner: Arc::new(Backend::Live(MultiProgress::with_draw_target(
                    ProgressDrawTarget::term_like(Box::new(term)),
                ))),
            }
        }
    }

    fn captured(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn mode_resolution_matrix() {
        use ProgressMode::*;
        // auto splits on TTY-ness; on forces live either way; verbose/none ignore the terminal.
        assert_eq!(
            resolve_rendering(Auto, false, true),
            Rendering::Live { forced: false }
        );
        assert_eq!(resolve_rendering(Auto, false, false), Rendering::Plain);
        assert_eq!(
            resolve_rendering(On, false, true),
            Rendering::Live { forced: true }
        );
        assert_eq!(
            resolve_rendering(On, false, false),
            Rendering::Live { forced: true }
        );
        assert_eq!(resolve_rendering(Verbose, false, true), Rendering::Verbose);
        assert_eq!(resolve_rendering(Verbose, false, false), Rendering::Verbose);
        assert_eq!(resolve_rendering(Off, false, true), Rendering::Off);
        // -q always means off — including over an explicit mode that slipped past clap's
        // same-level conflict by sitting on the other side of the verb.
        assert_eq!(resolve_rendering(Auto, true, true), Rendering::Off);
        assert_eq!(resolve_rendering(Auto, true, false), Rendering::Off);
        assert_eq!(resolve_rendering(On, true, false), Rendering::Off);
    }

    #[test]
    fn begin_item_finish_lines_format() {
        assert_eq!(
            begin_line("npm", "querying npm advisories"),
            "[npm] querying npm advisories ..."
        );
        assert_eq!(
            item_line("install", 3, Some(12), "debug@4.4.0"),
            "[install] (3/12) debug@4.4.0"
        );
        assert_eq!(
            item_line("resolve", 37, None, "lodash@4.17.21"),
            "[resolve] (37) lodash@4.17.21"
        );
        assert_eq!(
            finish_line("npm", "querying npm advisories", "169 advisories", 1.23),
            "[npm] querying npm advisories ... 169 advisories (1.2s)"
        );
    }

    #[test]
    fn plain_task_prints_begin_and_finish_pair_only() {
        let (progress, buf) = Progress::text_capture(false);
        let task = progress.task("install", "installing packages");
        task.set_total(2);
        task.inc("a@1.0.0");
        task.inc("b@2.0.0");
        task.detail("transient state");
        assert_eq!(task.count(), 2);
        task.finish("2 packages");

        let out = captured(&buf);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(lines[0], "[install] installing packages ...");
        assert!(
            lines[1].starts_with("[install] installing packages ... 2 packages ("),
            "{out}"
        );
    }

    #[test]
    fn verbose_task_prints_one_line_per_item() {
        let (progress, buf) = Progress::text_capture(true);
        let task = progress.task("install", "installing packages");
        task.set_total(2);
        task.inc("a@1.0.0");
        task.detail("transient state"); // not an item — no line
        task.inc("b@2.0.0");
        task.finish("2 packages");

        let out = captured(&buf);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert_eq!(lines[0], "[install] installing packages ...");
        assert_eq!(lines[1], "[install] (1/2) a@1.0.0");
        assert_eq!(lines[2], "[install] (2/2) b@2.0.0");
        assert!(lines[3].starts_with("[install] installing packages ... 2 packages ("));
    }

    #[test]
    fn warn_serializes_with_text_lines() {
        // Warnings share the text sink's Mutex, so a worker-thread warning can never tear a
        // task line; they keep the npm-utils: prefix in every mode.
        let (progress, buf) = Progress::text_capture(false);
        let task = progress.task("resolve", "resolving dependency tree");
        progress.inner.warn_msg("something failed; retrying");
        task.finish("1 package");

        let lines: Vec<String> = captured(&buf).lines().map(str::to_string).collect();
        assert_eq!(lines[1], "npm-utils: something failed; retrying");
    }

    #[test]
    fn off_tasks_are_inert_including_drop_without_finish() {
        // Off: full lifecycle plus a dropped-unfinished task must not print or panic (cargo
        // captures stderr; this is a state-machine smoke test — counting still works).
        let progress = Progress::from_rendering(Rendering::Off);
        let task = progress.task("resolve", "resolving");
        task.inc("a@1.0.0");
        task.set_total(3);
        task.detail("x");
        assert_eq!(task.count(), 1);
        task.finish("done");
        let unfinished = progress.task("npm", "querying npm advisories");
        drop(unfinished);
    }

    #[test]
    fn live_task_renders_staged_headers() {
        let term = InMemoryTerm::new(10, 80);
        let progress = Progress::live_on(term.clone());
        let task = progress.task("install", "installing packages");
        assert!(
            term.contents().contains("[install] installing packages"),
            "{}",
            term.contents()
        );

        // First count upgrades the bare header to `(1)`; the detail line names the item.
        task.inc("a@1.0.0");
        assert!(
            term.contents()
                .contains("[install] (1) installing packages"),
            "{}",
            term.contents()
        );
        assert!(term.contents().contains("  a@1.0.0"), "{}", term.contents());

        // A learned total upgrades to `(1/3)`; a transient detail replaces the item text.
        task.set_total(3);
        assert!(
            term.contents()
                .contains("[install] (1/3) installing packages"),
            "{}",
            term.contents()
        );
        task.detail("fetching b, c");
        assert!(
            term.contents().contains("  fetching b, c"),
            "{}",
            term.contents()
        );
        task.finish("3 packages");
    }

    #[test]
    fn live_finish_prints_permanent_line_and_clears_block() {
        let term = InMemoryTerm::new(10, 80);
        let progress = Progress::live_on(term.clone());
        let task = progress.task("install", "installing packages");
        task.set_total(3);
        task.inc("a@1.0.0");
        task.finish("3 packages");

        let contents = term.contents();
        assert!(
            contents.contains("[install] installing packages ... 3 packages ("),
            "{contents}"
        );
        // The live block is gone — no counter header or dangling detail line survives.
        assert!(!contents.contains("(1/3)"), "{contents}");
        assert!(!contents.contains("a@1.0.0"), "{contents}");
    }

    #[test]
    fn live_drop_backstop_clears_without_a_permanent_line() {
        let term = InMemoryTerm::new(10, 80);
        let progress = Progress::live_on(term.clone());
        let task = progress.task("resolve", "resolving dependency tree");
        task.inc("a@1.0.0");
        drop(task); // an error propagating mid-task
        assert_eq!(term.contents().trim(), "", "{}", term.contents());
    }
}
