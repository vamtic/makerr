//! Child-process helpers for platform-specific window behaviour.

/// Windows process creation flag: run a console program without allocating a
/// console window. The GUI itself has no console (it detaches at startup), so
/// ffmpeg and hardware probes would otherwise flash a console on top of it.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Stops a spawned std child process from opening its own console window.
#[cfg(windows)]
pub fn hide_console_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Stops a spawned tokio child process from opening its own console window.
#[cfg(windows)]
pub fn hide_console_window_async(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op outside Windows, where child processes have no console window.
#[cfg(not(windows))]
pub fn hide_console_window(_cmd: &mut std::process::Command) {}

/// No-op outside Windows, where child processes have no console window.
#[cfg(not(windows))]
pub fn hide_console_window_async(_cmd: &mut tokio::process::Command) {}
