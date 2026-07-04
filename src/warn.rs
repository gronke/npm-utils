//! The crate-internal `npm-utils:` warning channel. Library code calls [`warn`] instead of
//! `eprintln!("npm-utils: …")` so a renderer (the CLI's live progress region) can reroute the
//! line above its display instead of having it torn mid-redraw; with no sink installed the
//! behavior is byte-identical to the old eprintln. Same once-per-process [`OnceLock`] pattern
//! as [`crate::download::set_timeouts`].

use std::sync::OnceLock;

type Sink = Box<dyn Fn(&str) + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();

/// Install the process-wide warning sink; the first caller wins and later calls are ignored, so
/// in-process tests tolerate an already-installed sink. The sink receives the message *without*
/// the `npm-utils: ` prefix and must expect calls from worker threads (download retries fire on
/// the resolver's prefetch pool).
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn set_warn_sink(sink: Sink) {
    let _ = SINK.set(sink);
}

/// Emit one `npm-utils:` warning: through the installed sink, else the default
/// `eprintln!("npm-utils: {msg}")`.
pub(crate) fn warn(msg: &str) {
    match SINK.get() {
        Some(sink) => sink(msg),
        None => eprintln!("npm-utils: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    #[test]
    fn warn_routes_through_the_installed_sink() {
        // The sink is process-wide, so this is the only test that installs one; other tests in
        // this binary may emit warnings into it concurrently, hence marker probes instead of
        // exact-count assertions (nothing else asserts on warning output).
        set_warn_sink(Box::new(|msg| {
            CAPTURED.lock().unwrap().push(msg.to_string())
        }));
        warn("warn-sink-probe-one");
        assert!(CAPTURED
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "warn-sink-probe-one"));

        // A second install is ignored — the first sink keeps receiving.
        set_warn_sink(Box::new(|_| {}));
        warn("warn-sink-probe-two");
        assert!(CAPTURED
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "warn-sink-probe-two"));
    }
}
