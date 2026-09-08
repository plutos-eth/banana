//! Repository-wide source hygiene, checked as a test so `cargo test` catches it on any
//! platform without needing a shell script.
//!
//! Two things live here:
//!
//! * **Encoding.** Every text file is UTF-8 with no byte-order mark. This is not
//!   pedantry: PowerShell 5.1's `Set-Content -Encoding utf8` writes a BOM and mangles
//!   non-ASCII on a read/write round trip, which silently corrupted three files during
//!   phase 0 and was caught only because `cargo fmt --check` noticed the BOM. A BOM in a
//!   `.rs` file is a rustfmt diff; a BOM in `Cargo.toml` or `tauri.conf.json` is a parse
//!   risk in some tools.
//!
//! * **No `f64` in the money path** (spec §12, PLAN.md D7). Money is `U256` and ratios
//!   are integer basis points. There is no compiler switch for this, so it is a test.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories that are not our source.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "scratch", // the bodkin reference clone
    "dist",
    ".idea",
    "data",
];

/// Extensions we treat as text and therefore expect to be clean UTF-8.
const TEXT_EXTS: &[&str] = &[
    "rs", "toml", "json", "md", "ts", "tsx", "js", "css", "html", "yml", "yaml", "ps1", "example",
    "lock",
];

fn repo_root() -> PathBuf {
    // crates/core -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/core")
        .to_path_buf()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_ref()) {
                walk(&path, out);
            }
        } else {
            out.push(path);
        }
    }
}

fn text_files() -> Vec<PathBuf> {
    let mut all = Vec::new();
    walk(&repo_root(), &mut all);
    all.into_iter()
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| TEXT_EXTS.contains(&e))
                .unwrap_or(false)
        })
        .collect()
}

fn rel(path: &Path) -> String {
    path.strip_prefix(repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[test]
fn every_text_file_is_utf8_without_a_bom() {
    let mut problems = Vec::new();

    for path in text_files() {
        let Ok(bytes) = fs::read(&path) else { continue };
        if bytes.is_empty() {
            continue;
        }
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            problems.push(format!("{}: has a UTF-8 BOM", rel(&path)));
            continue;
        }
        match String::from_utf8(bytes) {
            Err(_) => problems.push(format!("{}: is not valid UTF-8", rel(&path))),
            Ok(text) => {
                // The residue of a UTF-8 file decoded as cp1252 and re-encoded.
                for marker in ['\u{00e2}', '\u{20ac}', '\u{00c3}'] {
                    if text.contains(marker) {
                        problems.push(format!(
                            "{}: contains mojibake ({marker:?}); it was round-tripped through a \
                             non-UTF-8 encoding",
                            rel(&path)
                        ));
                        break;
                    }
                }
            }
        }
    }

    assert!(
        problems.is_empty(),
        "source encoding problems ({}):\n  {}\n\nOn Windows, avoid `Set-Content -Encoding utf8` \
         (PowerShell 5.1 writes a BOM). Use [System.IO.File]::WriteAllText with \
         UTF8Encoding($false), or edit the file directly.",
        problems.len(),
        problems.join("\n  ")
    );
}

#[test]
fn no_f64_in_the_money_path() {
    // Spec §12: "f64 creeping into curve math or price computation". Money is U256 and
    // every ratio is integer basis points. Floating point is permitted only for values
    // that are display-only and never feed a decision or a stored figure -- and such a
    // use must say so on the line via `f64-ok:`.
    let core_src = repo_root().join("crates").join("core").join("src");
    let mut all = Vec::new();
    walk(&core_src, &mut all);

    let mut problems = Vec::new();
    for path in all
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
    {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("f64") || code.contains("f32") {
                if line.contains("f64-ok:") {
                    continue;
                }
                problems.push(format!("{}:{}: {}", rel(path), n + 1, line.trim()));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "floating point found in banana-core ({}):\n  {}\n\nMoney is U256 and ratios are \
         integer basis points (spec §12). If a use really is display-only, mark the line \
         with `f64-ok:` and a reason.",
        problems.len(),
        problems.join("\n  ")
    );
}
