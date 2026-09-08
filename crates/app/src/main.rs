//! The desktop entrypoint.
//!
//! Four lines on purpose: everything worth testing lives in the library, which builds and
//! runs without a window.

#![forbid(unsafe_code)]
// A release build must not also open a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    banana_app::run();
}
