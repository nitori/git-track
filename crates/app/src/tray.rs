//! System tray icon and hiding / showing the main window (Windows only).
//!
//! gpui has no tray support, so this uses the `tray-icon` crate. Its hidden
//! message window lives on the main thread and is served by gpui's message
//! loop (`GetMessageW` for all windows of the thread). Clicks and menu
//! selections arrive in global handlers and are forwarded as [`Command`]s
//! through a channel that a foreground task of the app consumes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Show,
    /// Left button pressed on the tray icon (decides what the click does).
    Press,
    /// Left click on the tray icon: hide if the window was in front, else show.
    Toggle,
    Quit,
}

pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::Command;
    use futures::channel::mpsc::{UnboundedReceiver, unbounded};
    use gpui::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::Arc;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, IsIconic, IsWindowVisible, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_RESTORE,
        SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    const SHOW_ID: &str = "show";
    const QUIT_ID: &str = "quit";

    /// Icon resource ids, see `app.rc`.
    const ICON_NORMAL: u16 = 1;
    const ICON_ALERT: u16 = 2;

    pub struct Tray {
        icon: TrayIcon,
        normal: Icon,
        alert: Icon,
        alert_shown: bool,
        tooltip: String,
    }

    impl Tray {
        /// Creates the tray icon. Must be called on the main thread.
        pub fn new() -> Result<(Self, UnboundedReceiver<Command>), String> {
            let (tx, rx) = unbounded();

            let click_tx = tx.clone();
            TrayIconEvent::set_event_handler(Some(move |event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state,
                    ..
                } = event
                {
                    let _ = click_tx.unbounded_send(match button_state {
                        MouseButtonState::Down => Command::Press,
                        MouseButtonState::Up => Command::Toggle,
                    });
                }
            }));
            MenuEvent::set_event_handler(Some(Arc::new(move |event: MenuEvent| {
                let command = match event.id().as_ref() {
                    SHOW_ID => Command::Show,
                    QUIT_ID => Command::Quit,
                    _ => return,
                };
                let _ = tx.unbounded_send(command);
            })));

            let menu = Menu::new();
            menu.append_items(&[
                &MenuItem::with_id(SHOW_ID, "Show git-track", true, None),
                &PredefinedMenuItem::separator(),
                &MenuItem::with_id(QUIT_ID, "Quit", true, None),
            ])
            .map_err(|e| format!("cannot create tray menu: {e}"))?;

            let normal = load_icon(ICON_NORMAL)?;
            let alert = load_icon(ICON_ALERT)?;
            let icon = TrayIconBuilder::new()
                .with_icon(normal.clone())
                .with_tooltip("git-track")
                .with_menu(Box::new(menu))
                // Left click shows the window, right click opens the menu.
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| format!("cannot create tray icon: {e}"))?;

            let tray = Self {
                icon,
                normal,
                alert,
                alert_shown: false,
                tooltip: "git-track".into(),
            };
            Ok((tray, rx))
        }

        /// Updates tooltip and icon; cheap when nothing changed.
        pub fn update(&mut self, tooltip: &str, alert: bool) {
            if self.tooltip != tooltip {
                let _ = self.icon.set_tooltip(Some(tooltip));
                self.tooltip = tooltip.to_string();
            }
            if self.alert_shown != alert {
                let icon = if alert { &self.alert } else { &self.normal };
                let _ = self.icon.set_icon(Some(icon.clone()));
                self.alert_shown = alert;
            }
        }
    }

    fn load_icon(id: u16) -> Result<Icon, String> {
        // Load the small-icon size directly instead of letting the shell scale the 32px one.
        let size = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
        let size = (size.0 > 0 && size.1 > 0).then_some((size.0 as u32, size.1 as u32));
        Icon::from_resource(id, size).map_err(|e| format!("cannot load icon resource {id}: {e}"))
    }

    fn hwnd(window: &Window) -> Option<HWND> {
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
            _ => None,
        }
    }

    /// Hides the window (it disappears from the taskbar). Returns false if that failed.
    pub fn hide_window(window: &Window) -> bool {
        let Some(hwnd) = hwnd(window) else {
            return false;
        };
        unsafe { ShowWindow(hwnd, SW_HIDE) };
        true
    }

    /// Visible and not minimized.
    pub fn is_shown(window: &Window) -> bool {
        hwnd(window)
            .is_some_and(|hwnd| unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 })
    }

    /// Shows a hidden or minimized window and brings it to the front.
    pub fn show_window(window: &Window) {
        let Some(hwnd) = hwnd(window) else {
            return;
        };
        unsafe {
            ShowWindow(
                hwnd,
                if IsIconic(hwnd) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Command;
    use futures::channel::mpsc::UnboundedReceiver;
    use gpui::Window;

    pub struct Tray;

    impl Tray {
        pub fn new() -> Result<(Self, UnboundedReceiver<Command>), String> {
            Err("the tray icon is only supported on Windows".into())
        }

        pub fn update(&mut self, _tooltip: &str, _alert: bool) {}
    }

    pub fn hide_window(_window: &Window) -> bool {
        false
    }

    pub fn is_shown(_window: &Window) -> bool {
        true
    }

    pub fn show_window(window: &Window) {
        window.activate_window();
    }
}
