//! The IPC surface, checked from both ends.
//!
//! The frontend calls commands by string name and the backend registers them by symbol.
//! Nothing in either language checks that the two lists agree, so a renamed command is a
//! runtime error in a view nobody ran — the exact failure a test suite is supposed to
//! catch and the exact one a type system cannot.
//!
//! This reads both files and compares. Crude, and much better than nothing: it fails the
//! moment a command is added on one side only.

use std::collections::BTreeSet;

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// Names inside `tauri::generate_handler![ ... ]` in the backend's `run()`.
fn registered() -> BTreeSet<String> {
    let src = read("src/lib.rs");
    let start = src
        .find("generate_handler![")
        .expect("run() registers its commands with generate_handler!");
    let end = src[start..].find(']').expect("unterminated handler list") + start;
    src[start..end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix("commands::"))
        .map(|l| l.trim_end_matches(',').to_string())
        .collect()
}

/// Names passed to `invoke("...")` anywhere in the frontend.
fn invoked() -> BTreeSet<String> {
    let src = read("../../ui/src/ipc.ts");
    let mut out = BTreeSet::new();
    for (i, _) in src.match_indices("invoke<") {
        let rest = &src[i..];
        let Some(open) = rest.find("(\"") else {
            continue;
        };
        let Some(close) = rest[open + 2..].find('"') else {
            continue;
        };
        out.insert(rest[open + 2..open + 2 + close].to_string());
    }
    out
}

/// Event names the backend emits, and the ones the frontend listens for.
fn emitted() -> BTreeSet<String> {
    let src = read("src/indexing.rs");
    let mut out = BTreeSet::new();
    for (i, _) in src.match_indices("emit(") {
        let rest = &src[i..];
        let Some(open) = rest.find("\"") else {
            continue;
        };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        out.insert(rest[open + 1..open + 1 + close].to_string());
    }
    out
}

fn listened() -> BTreeSet<String> {
    let src = read("../../ui/src/ipc.ts");
    let mut out = BTreeSet::new();
    for (i, _) in src.match_indices("listen<") {
        let rest = &src[i..];
        let Some(open) = rest.find("(\"") else {
            continue;
        };
        let Some(close) = rest[open + 2..].find('"') else {
            continue;
        };
        out.insert(rest[open + 2..open + 2 + close].to_string());
    }
    out
}

#[test]
fn every_command_the_ui_calls_is_registered_by_the_backend() {
    let registered = registered();
    let invoked = invoked();
    assert!(!registered.is_empty(), "found no registered commands");
    assert!(!invoked.is_empty(), "found no invoke() calls");

    let missing: Vec<_> = invoked.difference(&registered).collect();
    assert!(
        missing.is_empty(),
        "the UI calls commands the backend does not register: {missing:?}"
    );
}

/// The other direction, as a tidiness check rather than a correctness one.
#[test]
fn every_registered_command_is_reachable_from_the_ui() {
    let unused: Vec<_> = registered().difference(&invoked()).cloned().collect();
    assert!(
        unused.is_empty(),
        "these commands are registered but nothing calls them, which means either a dead \
         entry in the trust surface or a view that was never wired up: {unused:?}"
    );
}

#[test]
fn every_event_the_backend_emits_is_listened_for() {
    let emitted = emitted();
    let listened = listened();
    assert!(!emitted.is_empty(), "found no emitted events");
    assert_eq!(
        emitted, listened,
        "the index progress events and the listeners for them have drifted apart"
    );
}

/// The list is the trust surface, so its size is worth noticing when it changes.
#[test]
fn the_ipc_surface_is_small_enough_to_read_in_one_sitting() {
    let n = registered().len();
    assert!(
        n <= 16,
        "{n} commands. Every one of these is something a compromised frontend could \
         cause; if the surface is growing, the growth should be deliberate."
    );
}
