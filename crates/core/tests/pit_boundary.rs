//! The point-in-time boundary is a compile error, not a comment.
//!
//! Spec §5.3 says to put post-entry facts in a different Rust type so the compiler
//! enforces it. That is only a real guarantee if something checks the code still fails to
//! compile; otherwise a future signature change quietly removes it and every test still
//! passes. `trybuild` runs the fixture in `tests/ui/` and asserts it does not build.

#[test]
fn a_filter_cannot_read_post_entry_facts() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/post_entry_facts_are_unreachable_from_a_filter.rs");
}
