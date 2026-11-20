//! TimelapseMaker — one binary, two front ends.
//!
//! `timelapse <subcommand> ...` runs headless (scripts, CI, remote boxes);
//! `timelapse` with no arguments opens the Slint desktop GUI. Both modes run
//! on the same `tlm-core`, so settings files, templates and output folders
//! behave identically either way.
//!
//! Exit codes: 0 success · 1 cancelled/failed · 2 invalid input.

mod capacity;
mod cli;
mod gui;

// The generated Slint types are shared by `gui` (which uses them) and kept at
// the crate root because that is where the macro expects to expand them.
slint::include_modules!();

fn main() {
    // clap handles `--help` / `--version` and exits by itself; a subcommand
    // returns an exit code, while no subcommand means "start the GUI".
    if let Some(code) = cli::dispatch() {
        std::process::exit(code);
    }

    if !display_available() {
        eprintln!(
            "timelapse: no graphical session found (DISPLAY / WAYLAND_DISPLAY are unset).\n\
             Run `timelapse --help` to use the headless commands instead."
        );
        std::process::exit(2);
    }

    if let Err(err) = gui::run() {
        eprintln!("timelapse: failed to start the GUI: {err}");
        std::process::exit(1);
    }
}

/// Windows and macOS always have a window server; on Linux the GUI needs an
/// X11 or Wayland session.
fn display_available() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return true;
    }
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
