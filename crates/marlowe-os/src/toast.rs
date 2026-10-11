//! Windows toast notifications for `notify` (BG7): `ToastNotificationManager` with an unpackaged
//! sender id, `Marlowe.Cowork`, written once under `HKCU\Software\Classes\AppUserModelId` at the
//! first toast (no installer, no shortcut; design 9.1-k).
//!
//! Seam stub: empty until track N lands. The `windows` features it needs (`UI_Notifications`,
//! `Data_Xml_Dom`, `Win32_System_Registry`) are enabled and the dependency is declared, so N edits
//! only this file. Tests use a fake toaster; no test shows a real toast or writes the registry.
