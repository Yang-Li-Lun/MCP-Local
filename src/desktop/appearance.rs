use slint::{Color, ComponentHandle};
use windows_sys::Win32::{
    Graphics::Gdi::{
        COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
        COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor,
    },
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW},
    },
};

use super::{
    AboutWindow, Desktop, MonitorDiagnosticsWindow, MonitorWindow, SystemContrastColors,
    WindowsContrast,
};

fn colorref(value: u32) -> Color {
    let [red, green, blue, _] = value.to_le_bytes();
    Color::from_rgb_u8(red, green, blue)
}

fn windows_colors() -> Option<SystemContrastColors> {
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            (&mut contrast as *mut HIGHCONTRASTW).cast(),
            0,
        )
    };
    if read == 0 {
        return None;
    }
    if contrast.dwFlags & HCF_HIGHCONTRASTON == 0 {
        return Some(SystemContrastColors::default());
    }
    let get = |index| colorref(unsafe { GetSysColor(index) });
    Some(SystemContrastColors {
        active: true,
        window_color: get(COLOR_WINDOW),
        window_text: get(COLOR_WINDOWTEXT),
        button_face: get(COLOR_BTNFACE),
        button_text: get(COLOR_BTNTEXT),
        highlight: get(COLOR_HIGHLIGHT),
        highlight_text: get(COLOR_HIGHLIGHTTEXT),
        gray_text: get(COLOR_GRAYTEXT),
    })
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut std::ffi::c_void,
        size: *mut u32,
    ) -> i32;
}

fn personalization(name: &str) -> Option<u32> {
    let path: Vec<u16> = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"
        .encode_utf16()
        .chain([0])
        .collect();
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            -2147483647isize,
            path.as_ptr(),
            name.as_ptr(),
            0x10,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    (result == 0 && size == size_of::<u32>() as u32).then_some(value)
}

fn tint_allowed(colors: Option<&SystemContrastColors>, transparency: Option<u32>) -> bool {
    colors.is_some_and(|colors| !colors.active) && transparency == Some(1)
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attribute: u32, value: *const u32, size: u32) -> i32;
}

fn set_attribute(hwnd: isize, attribute: u32, value: u32) -> bool {
    unsafe { DwmSetWindowAttribute(hwnd, attribute, &value, size_of::<u32>() as u32) >= 0 }
}

#[derive(Default)]
pub(super) struct Appearance {
    colors: Option<SystemContrastColors>,
    backdrop: Option<(isize, bool, bool)>,
}

impl Appearance {
    pub(super) fn refresh(
        &mut self,
        ui: &Desktop,
        monitor: &MonitorWindow,
        about: &AboutWindow,
        diagnostics: [&MonitorDiagnosticsWindow; 2],
    ) {
        let Some(colors) = windows_colors() else {
            return;
        };
        if self.colors.as_ref() != Some(&colors) {
            self.colors = Some(colors.clone());
            apply(ui, monitor, about, &diagnostics, colors);
        }
    }

    pub(super) fn refresh_backdrop(&mut self, ui: &Desktop) {
        let Some(hwnd) = super::hwnd(ui.window()) else {
            return;
        };
        let allowed = tint_allowed(self.colors.as_ref(), personalization("EnableTransparency"));
        let dark = ui.get_dark_palette();
        let state = (hwnd, allowed, dark);
        if self.backdrop == Some(state) {
            return;
        }
        self.backdrop = Some(state);
        const IMMERSIVE_DARK_MODE: u32 = 20;
        const WINDOW_CORNER_PREFERENCE: u32 = 33;
        const SYSTEM_BACKDROP_TYPE: u32 = 38;
        set_attribute(hwnd, IMMERSIVE_DARK_MODE, dark.into());
        set_attribute(hwnd, WINDOW_CORNER_PREFERENCE, 2); // DWMWCP_ROUND
        // Slint's winit surface already supplies per-pixel alpha. Extending the
        // non-client frame over it would expose the system caption and buttons.
        let applied = set_attribute(hwnd, SYSTEM_BACKDROP_TYPE, if allowed { 2 } else { 1 });
        ui.set_mica_enabled(allowed && applied);
    }
}

pub(super) fn apply(
    ui: &Desktop,
    monitor: &MonitorWindow,
    about: &AboutWindow,
    diagnostics: &[&MonitorDiagnosticsWindow],
    colors: SystemContrastColors,
) {
    ui.global::<WindowsContrast>().set_colors(colors.clone());
    monitor
        .global::<WindowsContrast>()
        .set_colors(colors.clone());
    about.global::<WindowsContrast>().set_colors(colors.clone());
    for dialog in diagnostics {
        dialog
            .global::<WindowsContrast>()
            .set_colors(colors.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_colorref_has_red_in_the_low_byte() {
        assert_eq!(colorref(0x00402010), Color::from_rgb_u8(0x10, 0x20, 0x40));
    }

    #[test]
    fn reads_the_current_windows_contrast_setting() {
        assert!(windows_colors().is_some());
    }

    #[test]
    fn tint_requires_transparency_and_confirmed_non_high_contrast() {
        let normal = SystemContrastColors::default();
        let high_contrast = SystemContrastColors {
            active: true,
            ..normal.clone()
        };
        assert!(tint_allowed(Some(&normal), Some(1)));
        assert!(!tint_allowed(Some(&normal), Some(0)));
        assert!(!tint_allowed(Some(&normal), None));
        assert!(!tint_allowed(None, Some(1)));
        assert!(!tint_allowed(Some(&high_contrast), Some(1)));
    }
}
