//! "Start with Windows": a value under `HKCU\…\CurrentVersion\Run`.
//!
//! The command line carries [`ARG`], so an automatic start can open hidden
//! in the tray instead of popping up the window at login.

/// Marks a start through the Run key.
pub const ARG: &str = "--autostart";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Off,
    /// Starts this executable.
    On,
    /// Starts a different copy (e.g. a release build while this is a debug build).
    Other(String),
}

/// True if this process was started through the Run key.
pub fn launched() -> bool {
    std::env::args().skip(1).any(|a| a == ARG)
}

pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::{ARG, State};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "git-track";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn exe() -> Result<String, String> {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cannot determine the executable path: {e}"))
    }

    /// The executable a Run command line starts (quoted or not).
    fn command_exe(command: &str) -> &str {
        let command = command.trim();
        match command.strip_prefix('"') {
            Some(rest) => rest.split('"').next().unwrap_or(rest),
            None => command.split(' ').next().unwrap_or(command),
        }
    }

    fn read() -> Result<Option<String>, String> {
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        let mut size = 0u32;
        let query = |data: *mut core::ffi::c_void, size: &mut u32| unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                data,
                size,
            )
        };
        match query(std::ptr::null_mut(), &mut size) {
            ERROR_SUCCESS => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            code => return Err(format!("cannot read the autostart entry (error {code})")),
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2)];
        match query(buf.as_mut_ptr().cast(), &mut size) {
            ERROR_SUCCESS => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            code => return Err(format!("cannot read the autostart entry (error {code})")),
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..len])))
    }

    pub fn state() -> State {
        let Ok(Some(command)) = read() else {
            return State::Off;
        };
        let target = command_exe(&command);
        match exe() {
            Ok(exe) if target.eq_ignore_ascii_case(&exe) => State::On,
            _ => State::Other(target.to_string()),
        }
    }

    /// Registers this executable, or removes the entry.
    pub fn set(enabled: bool) -> Result<(), String> {
        let (key, value) = (wide(RUN_KEY), wide(VALUE));
        let code = if enabled {
            let data = wide(&format!("\"{}\" {ARG}", exe()?));
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            }
        } else {
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
                ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                code => code,
            }
        };
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("cannot change the autostart entry (error {code})"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::command_exe;

        #[test]
        fn extracts_the_executable() {
            assert_eq!(
                command_exe(r#""C:\Program Files\git-track\git-track.exe" --autostart"#),
                r"C:\Program Files\git-track\git-track.exe"
            );
            assert_eq!(
                command_exe(r"C:\x\git-track.exe --autostart"),
                r"C:\x\git-track.exe"
            );
            assert_eq!(command_exe(r"C:\x\git-track.exe"), r"C:\x\git-track.exe");
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::State;

    pub fn state() -> State {
        State::Off
    }

    pub fn set(_enabled: bool) -> Result<(), String> {
        Err("autostart is only supported on Windows".into())
    }
}
