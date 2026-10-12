//! The notification-area icon shown while Marlowe keeps running after its window closed (the
//! opt-in "keep running" choice). Closing the window otherwise stops everything, so this icon,
//! with the `[Stop now]` toast, is how a background Marlowe is never invisible.
//!
//! [`Tray`] is the port. [`WindowsTray`] owns a message-only window on its own thread (no window
//! is ever shown; only the icon) with a one-item menu, `Stop Marlowe now`. [`FakeTray`] records
//! calls and is what every test uses: no test puts an icon on the desktop.

use std::sync::{Arc, Mutex};

use crate::toast::ActionHandler;
use crate::OsError;

/// The id the stop menu item passes to the handler.
pub const STOP: &str = "stop";

/// Shows and hides the icon.
pub trait Tray: Send + Sync {
    /// Puts the icon up with `tooltip`. Showing twice only updates nothing: it stays one icon.
    fn show(&self, tooltip: &str) -> Result<(), OsError>;
    /// Takes the icon down. Idempotent.
    fn hide(&self);
    fn shown(&self) -> bool;
    /// Sets the handler for the icon's menu; it is called with [`STOP`].
    fn on_action(&self, handler: ActionHandler);
}

/// A tray that records.
#[derive(Default)]
pub struct FakeTray {
    state: Mutex<(bool, Vec<String>)>,
    handler: Mutex<Option<ActionHandler>>,
}

impl FakeTray {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every tooltip it was shown with.
    pub fn tooltips(&self) -> Vec<String> {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).1.clone()
    }

    /// Chooses the menu item `id`; false when no handler is set.
    pub fn choose(&self, id: &str) -> bool {
        let h = self.handler.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match h {
            Some(h) => {
                h(id);
                true
            }
            None => false,
        }
    }
}

impl Tray for FakeTray {
    fn show(&self, tooltip: &str) -> Result<(), OsError> {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !g.0 {
            g.0 = true;
            g.1.push(tooltip.to_string());
        }
        Ok(())
    }

    fn hide(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).0 = false;
    }

    fn shown(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).0
    }

    fn on_action(&self, handler: ActionHandler) {
        *self.handler.lock().unwrap_or_else(|p| p.into_inner()) = Some(handler);
    }
}

/// The real tray icon. Off Windows `show` reports `Unsupported`.
#[derive(Default)]
pub struct WindowsTray {
    #[cfg(windows)]
    running: Mutex<Option<win::Running>>,
}

impl WindowsTray {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Tray for WindowsTray {
    fn show(&self, tooltip: &str) -> Result<(), OsError> {
        #[cfg(windows)]
        {
            let mut g = self.running.lock().unwrap_or_else(|p| p.into_inner());
            if g.is_none() {
                *g = Some(win::start(tooltip)?);
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = tooltip;
            Err(OsError::Unsupported("the tray icon"))
        }
    }

    fn hide(&self) {
        #[cfg(windows)]
        if let Some(r) = self.running.lock().unwrap_or_else(|p| p.into_inner()).take() {
            r.stop();
        }
    }

    fn shown(&self) -> bool {
        #[cfg(windows)]
        {
            self.running.lock().unwrap_or_else(|p| p.into_inner()).is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn on_action(&self, handler: ActionHandler) {
        #[cfg(windows)]
        {
            *win::HANDLER.lock().unwrap_or_else(|p| p.into_inner()) = Some(handler);
        }
        #[cfg(not(windows))]
        let _ = handler;
    }
}

impl Drop for WindowsTray {
    fn drop(&mut self) {
        self.hide();
    }
}

#[cfg(windows)]
mod win {
    use std::sync::mpsc::channel;
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
        DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW,
        SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HWND_MESSAGE, ICONINFO, MF_STRING, MSG,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
        WNDCLASSW,
    };

    use crate::toast::ActionHandler;
    use crate::OsError;

    /// The handler the menu calls (the window procedure is a bare function, so it is global).
    pub(super) static HANDLER: Mutex<Option<ActionHandler>> = Mutex::new(None);

    const WM_TRAY: u32 = WM_APP + 1;
    const WM_REMOVE: u32 = WM_APP + 2;
    const ID_STOP: usize = 1;

    pub(super) struct Running {
        hwnd: isize,
        thread: JoinHandle<()>,
    }

    impl Running {
        /// Removes the icon and ends the thread.
        pub(super) fn stop(self) {
            // SAFETY: `hwnd` is the message-only window that thread created; posting to a window
            // that is already gone only fails.
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_REMOVE, WPARAM(0), LPARAM(0));
            }
            let _ = self.thread.join();
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn start(tooltip: &str) -> Result<Running, OsError> {
        let (tx, rx) = channel::<Result<isize, OsError>>();
        let tip = tooltip.to_string();
        let thread = std::thread::Builder::new().name("marlowe-tray".into()).spawn(move || run(&tip, tx))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(hwnd)) => Ok(Running { hwnd, thread }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(OsError::Unsupported("the tray icon (the shell did not answer)")),
        }
    }

    /// The accent disc, `#9B7EDE` (theme.rs), 16 x 16, transparent outside.
    fn make_icon() -> Option<HICON> {
        const N: i32 = 16;
        let mut bgra = Vec::with_capacity((N * N * 4) as usize);
        for y in 0..N {
            for x in 0..N {
                let (dx, dy) = (x as f32 - 7.5, y as f32 - 7.5);
                if dx * dx + dy * dy <= 7.0 * 7.0 {
                    bgra.extend_from_slice(&[0xDE, 0x7E, 0x9B, 0xFF]);
                } else {
                    bgra.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        // SAFETY: both bitmaps are created from buffers that outlive the calls (the colour bits
        // are copied by CreateBitmap), the icon owns copies, and the bitmaps are deleted after.
        unsafe {
            let colour = CreateBitmap(N, N, 1, 32, Some(bgra.as_ptr() as *const _));
            let mask = CreateBitmap(N, N, 1, 1, None);
            let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: colour };
            let icon = CreateIconIndirect(&info).ok();
            let _ = DeleteObject(HGDIOBJ(colour.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
            icon
        }
    }

    fn run(tooltip: &str, ready: std::sync::mpsc::Sender<Result<isize, OsError>>) {
        // SAFETY: plain Win32 calls on this thread, which owns the window, the icon and the
        // message loop; every handle created here is destroyed before the thread ends.
        unsafe {
            let fail = |call: &'static str| {
                let _ = ready.send(Err(OsError::Win32 { call, code: windows::core::Error::from_thread().code().0 as u32 }));
            };
            let Ok(module) = GetModuleHandleW(PCWSTR::null()) else { return fail("GetModuleHandleW") };
            let hinst: HINSTANCE = module.into();
            let class = wide("MarloweTrayWindow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            // Zero when the class exists already (the icon was shown and hidden before).
            let _ = RegisterClassW(&wc);
            let empty = wide("");
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(empty.as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinst),
                None,
            ) {
                Ok(h) => h,
                Err(_) => return fail("CreateWindowExW"),
            };
            let icon = make_icon();
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: icon.unwrap_or_default(),
                ..Default::default()
            };
            for (slot, unit) in nid.szTip.iter_mut().zip(tooltip.encode_utf16().take(127)) {
                *slot = unit;
            }
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                let _ = DestroyWindow(hwnd);
                if let Some(i) = icon {
                    let _ = DestroyIcon(i);
                }
                return fail("Shell_NotifyIconW");
            }
            let _ = ready.send(Ok(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            if let Some(i) = icon {
                let _ = DestroyIcon(i);
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_TRAY => {
                let event = (lparam.0 as u32) & 0xFFFF;
                if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                    // SAFETY: called on the window's own thread with its handle.
                    unsafe { menu(hwnd) };
                }
                LRESULT(0)
            }
            WM_REMOVE => {
                // SAFETY: destroys this thread's own window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            // SAFETY: the default procedure for messages this window does not handle.
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// The one-item menu at the cursor; choosing it calls the handler off this thread.
    unsafe fn menu(hwnd: HWND) {
        // SAFETY: the menu is created, shown and destroyed here, on the window's thread.
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let label = wide("Stop Marlowe now");
            let _ = AppendMenuW(menu, MF_STRING, ID_STOP, PCWSTR(label.as_ptr()));
            let mut at = POINT::default();
            let _ = GetCursorPos(&mut at);
            // The menu closes when the user clicks elsewhere only if its owner is foreground.
            let _ = SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None);
            let _ = DestroyMenu(menu);
            if chosen.0 as usize == ID_STOP {
                let h = HANDLER.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if let Some(h) = h {
                    std::thread::spawn(move || h(super::STOP));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_shows_once_hides_and_plays_menu_choices() {
        let t = FakeTray::new();
        let chosen = Arc::new(Mutex::new(Vec::new()));
        let c = chosen.clone();
        t.on_action(Arc::new(move |id| c.lock().unwrap().push(id.to_string())));
        assert!(!t.shown());
        t.show("Marlowe is running").unwrap();
        t.show("Marlowe is running").unwrap();
        assert!(t.shown());
        assert_eq!(t.tooltips().len(), 1, "one icon, however often it is asked for");
        assert!(t.choose(STOP));
        assert_eq!(*chosen.lock().unwrap(), [STOP]);
        t.hide();
        assert!(!t.shown());
    }
}
