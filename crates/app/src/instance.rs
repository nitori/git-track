//! Single-instance handling (Windows only).
//!
//! The first instance owns a named mutex and waits on a named event in a
//! background thread. A second instance signals that event (which makes the
//! first one show its window) and exits. Both names include a hash of the
//! executable path, so e.g. a debug and a release build do not interfere.

use futures::channel::mpsc::UnboundedReceiver;

use crate::tray::Command;

pub enum Instance {
    /// This is the only instance; receives `Command::Show` whenever another
    /// instance was started.
    Primary(UnboundedReceiver<Command>),
    /// Another instance is running and was asked to show itself.
    Secondary,
}

pub use imp::acquire;

#[cfg(windows)]
mod imp {
    use super::{Command, Instance};
    use futures::channel::mpsc::unbounded;
    use std::hash::{DefaultHasher, Hash, Hasher};
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Never fails: if the kernel objects cannot be created, the app simply
    /// runs without single-instance handling.
    pub fn acquire() -> Instance {
        let (tx, rx) = unbounded();

        let mut hasher = DefaultHasher::new();
        std::env::current_exe()
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_default()
            .hash(&mut hasher);
        let id = format!("{:016x}", hasher.finish());
        let mutex_name = wide(&format!("Local\\git-track-instance-{id}"));
        let event_name = wide(&format!("Local\\git-track-show-{id}"));

        unsafe {
            // Kept open for the lifetime of the process; Windows closes it on exit.
            let mutex = CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr());
            if mutex.is_null() {
                return Instance::Primary(rx);
            }
            let already_running = GetLastError() == ERROR_ALREADY_EXISTS;

            // Auto-reset event; opens the existing one in the second instance.
            let event = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr());
            if event.is_null() {
                return Instance::Primary(rx);
            }

            if already_running {
                // We were started by the user and may take the foreground;
                // pass that right on so the first instance can activate itself.
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(event);
                return Instance::Secondary;
            }

            // HANDLE is a raw pointer (not Send); move it as an integer.
            let event = event as usize;
            std::thread::Builder::new()
                .name("git-track-instance".into())
                .spawn(move || {
                    loop {
                        WaitForSingleObject(event as _, INFINITE);
                        if tx.unbounded_send(Command::Show).is_err() {
                            break;
                        }
                    }
                })
                .ok();
        }
        Instance::Primary(rx)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Instance;

    pub fn acquire() -> Instance {
        Instance::Primary(futures::channel::mpsc::unbounded().1)
    }
}
