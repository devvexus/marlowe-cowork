//! Windows toast notifications for `notify` (BG7): `ToastNotificationManager` with an unpackaged
//! sender id, [`APP_ID`] (`Marlowe.Cowork`), written once under
//! `HKCU\Software\Classes\AppUserModelId` at the first toast (no installer, no shortcut; design
//! 9.1-k).
//!
//! Callers hold a [`Toaster`]. [`WindowsToaster`] is the real one; [`FakeToaster`] records what it
//! was asked to show, and every test uses it: no test shows a real toast or writes the registry.
//! The toast XML is built by [`toast_xml`], a pure function the tests read.
//!
//! A toast may carry buttons ([`ToastAction`]). Pressing one calls the handler set with
//! [`Toaster::on_action`] with the button's `id`, as long as this process runs (the daemon does:
//! the `[Stop now]` toast shown while Marlowe keeps running in the background).

use std::sync::{Arc, Mutex};

use crate::OsError;

/// The sender id toasts are shown under (the "app name" in Action Center).
pub const APP_ID: &str = "Marlowe.Cowork";
/// The display name stored under the id's registry key.
pub const APP_NAME: &str = "Marlowe";

/// One button on a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastAction {
    pub label: String,
    /// What the handler receives when it is pressed.
    pub id: String,
}

/// What to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub body: String,
    /// Stays on screen until dismissed (the `reminder` scenario; needs a button to take effect).
    pub urgent: bool,
    pub actions: Vec<ToastAction>,
    /// Replaces an earlier toast with the same tag instead of stacking.
    pub tag: Option<String>,
}

/// Called with the pressed button's id.
pub type ActionHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// Shows toasts. Implementations must not block for long.
pub trait Toaster: Send + Sync {
    fn show(&self, toast: &Toast) -> Result<(), OsError>;

    /// Sets the handler for button presses on toasts this toaster showed.
    fn on_action(&self, _handler: ActionHandler) {}
}

/// `text` with the five XML specials escaped.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters outright.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// The toast's XML document (the `ToastGeneric` template).
pub fn toast_xml(toast: &Toast) -> String {
    let scenario = if toast.urgent && !toast.actions.is_empty() { " scenario=\"reminder\"" } else { "" };
    let mut xml = format!("<toast{scenario}><visual><binding template=\"ToastGeneric\">");
    xml.push_str(&format!("<text>{}</text>", xml_escape(toast.title.trim())));
    if !toast.body.trim().is_empty() {
        xml.push_str(&format!("<text>{}</text>", xml_escape(toast.body.trim())));
    }
    xml.push_str("</binding></visual>");
    if !toast.actions.is_empty() {
        xml.push_str("<actions>");
        for a in &toast.actions {
            xml.push_str(&format!(
                "<action content=\"{}\" arguments=\"{}\" activationType=\"foreground\"/>",
                xml_escape(&a.label),
                xml_escape(&a.id)
            ));
        }
        xml.push_str("</actions>");
    }
    xml.push_str("</toast>");
    xml
}

/// A toaster that records instead of showing. `press` plays a button press into the handler.
#[derive(Default)]
pub struct FakeToaster {
    shown: Mutex<Vec<Toast>>,
    handler: Mutex<Option<ActionHandler>>,
    /// Makes `show` fail (a machine with notifications blocked).
    pub fail: std::sync::atomic::AtomicBool,
}

impl FakeToaster {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Every toast shown so far.
    pub fn shown(&self) -> Vec<Toast> {
        self.shown.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The XML of every toast shown.
    pub fn xml(&self) -> Vec<String> {
        self.shown().iter().map(toast_xml).collect()
    }

    /// Presses the button `id`; false when no handler is set.
    pub fn press(&self, id: &str) -> bool {
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

impl Toaster for FakeToaster {
    fn show(&self, toast: &Toast) -> Result<(), OsError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(OsError::Unsupported("toast notifications"));
        }
        self.shown.lock().unwrap_or_else(|p| p.into_inner()).push(toast.clone());
        Ok(())
    }

    fn on_action(&self, handler: ActionHandler) {
        *self.handler.lock().unwrap_or_else(|p| p.into_inner()) = Some(handler);
    }
}

/// The real toaster. Off Windows it reports `Unsupported`.
pub struct WindowsToaster {
    handler: Arc<Mutex<Option<ActionHandler>>>,
    registered: std::sync::Once,
}

impl Default for WindowsToaster {
    fn default() -> Self {
        Self { handler: Arc::default(), registered: std::sync::Once::new() }
    }
}

impl WindowsToaster {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Toaster for WindowsToaster {
    fn show(&self, toast: &Toast) -> Result<(), OsError> {
        if crate::desktop_quiet() {
            return Ok(());
        }
        #[cfg(windows)]
        {
            self.registered.call_once(|| {
                // Best effort: without the key Windows still shows the toast, under a generic name.
                let _ = win::ensure_app_id();
            });
            win::show(toast, &self.handler)
        }
        #[cfg(not(windows))]
        {
            let _ = (toast, &self.registered, &self.handler);
            Err(OsError::Unsupported("toast notifications"))
        }
    }

    fn on_action(&self, handler: ActionHandler) {
        *self.handler.lock().unwrap_or_else(|p| p.into_inner()) = Some(handler);
    }
}

#[cfg(windows)]
mod win {
    use std::sync::{Arc, Mutex};

    use windows::core::{Interface, HSTRING};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    use super::{toast_xml, ActionHandler, Toast, APP_ID, APP_NAME};
    use crate::OsError;

    fn werr(call: &'static str, e: windows::core::Error) -> OsError {
        OsError::Win32 { call, code: e.code().0 as u32 }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn key_path() -> String {
        format!(r"Software\Classes\AppUserModelId\{APP_ID}")
    }

    /// Writes `HKCU\Software\Classes\AppUserModelId\Marlowe.Cowork\DisplayName` unless the key
    /// exists. The only registry write the toast makes (design 9.1-k).
    pub(super) fn ensure_app_id() -> Result<(), OsError> {
        let path = wide(&key_path());
        let mut key = HKEY::default();
        // SAFETY: `path` is NUL-terminated and outlives the calls; `key` is a valid out pointer
        // and is closed below whenever a call opened it.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, windows::core::PCWSTR(path.as_ptr()), None, KEY_READ, &mut key) == ERROR_SUCCESS {
                let _ = RegCloseKey(key);
                return Ok(());
            }
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(path.as_ptr()),
                None,
                windows::core::PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            );
            if rc != ERROR_SUCCESS {
                return Err(OsError::Win32 { call: "RegCreateKeyExW", code: rc.0 });
            }
            let name = wide("DisplayName");
            let value = wide(APP_NAME);
            // REG_SZ wants the UTF-16 bytes including the terminating NUL, which `wide` appended.
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            let rc = RegSetValueExW(key, windows::core::PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes));
            let _ = RegCloseKey(key);
            if rc != ERROR_SUCCESS {
                return Err(OsError::Win32 { call: "RegSetValueExW", code: rc.0 });
            }
        }
        Ok(())
    }

    pub(super) fn show(toast: &Toast, handler: &Arc<Mutex<Option<ActionHandler>>>) -> Result<(), OsError> {
        let doc = XmlDocument::new().map_err(|e| werr("XmlDocument", e))?;
        doc.LoadXml(&HSTRING::from(toast_xml(toast))).map_err(|e| werr("LoadXml", e))?;
        let note = ToastNotification::CreateToastNotification(&doc).map_err(|e| werr("CreateToastNotification", e))?;
        if let Some(tag) = &toast.tag {
            // Tags are limited to 64 characters.
            let tag: String = tag.chars().take(64).collect();
            let _ = note.SetTag(&HSTRING::from(tag));
        }
        if !toast.actions.is_empty() {
            let handler = handler.clone();
            let activated = TypedEventHandler::new(move |_sender, args: windows::core::Ref<'_, windows::core::IInspectable>| {
                let id = args
                    .as_ref()
                    .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|a| a.Arguments().ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let h = handler.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if let (Some(h), false) = (h, id.is_empty()) {
                    h(&id);
                }
                Ok(())
            });
            note.Activated(&activated).map_err(|e| werr("Activated", e))?;
        }
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
            .map_err(|e| werr("CreateToastNotifierWithId", e))?;
        notifier.Show(&note).map_err(|e| werr("Show", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_xml_escapes_everything_the_model_wrote() {
        let t = Toast {
            title: "run <b>done</b> & \"ok\"".into(),
            body: "it's 5 > 3\u{1}".into(),
            ..Toast::default()
        };
        let xml = toast_xml(&t);
        assert!(xml.contains("<text>run &lt;b&gt;done&lt;/b&gt; &amp; &quot;ok&quot;</text>"), "{xml}");
        assert!(xml.contains("<text>it&apos;s 5 &gt; 3</text>"), "{xml}");
        assert!(!xml.contains("<b>") && !xml.contains('\u{1}'));
    }

    #[test]
    fn buttons_and_the_urgent_scenario() {
        let mut t = Toast { title: "Marlowe is running in the background".into(), urgent: true, ..Toast::default() };
        assert!(!toast_xml(&t).contains("scenario"), "urgent without a button is an ordinary toast");
        t.actions.push(ToastAction { label: "Stop now".into(), id: "stop".into() });
        let xml = toast_xml(&t);
        assert!(xml.starts_with("<toast scenario=\"reminder\">"), "{xml}");
        assert!(xml.contains("<action content=\"Stop now\" arguments=\"stop\" activationType=\"foreground\"/>"), "{xml}");
        assert!(!xml.contains("<text></text>"), "an empty body is left out");
    }

    #[test]
    fn the_fake_records_and_plays_button_presses() {
        let f = FakeToaster::new();
        let pressed = Arc::new(Mutex::new(Vec::new()));
        let p = pressed.clone();
        f.on_action(Arc::new(move |id| p.lock().unwrap().push(id.to_string())));
        f.show(&Toast { title: "a".into(), ..Toast::default() }).unwrap();
        assert!(f.press("stop"));
        assert_eq!(f.shown().len(), 1);
        assert_eq!(*pressed.lock().unwrap(), ["stop"]);
        f.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(f.show(&Toast::default()).is_err());
    }
}
