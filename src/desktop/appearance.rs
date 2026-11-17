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

#[repr(C)]
struct Margins {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: isize,
        attribute: u32,
        value: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn DwmExtendFrameIntoClientArea(hwnd: isize, margins: *const Margins) -> i32;
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

fn mica_allowed(colors: Option<&SystemContrastColors>, transparency: Option<u32>) -> bool {
    colors.is_some_and(|colors| !colors.active) && transparency == Some(1)
}

#[derive(Default)]
pub(super) struct Appearance {
    colors: Option<SystemContrastColors>,
    mica: Option<(isize, bool, bool)>,
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

    pub(super) fn refresh_mica(&mut self, ui: &Desktop) {
        let Some(hwnd) = super::hwnd(ui.window()) else {
            return;
        };
        let allowed = mica_allowed(self.colors.as_ref(), personalization("EnableTransparency"));
        let dark = match ui.get_theme() {
            1 => false,
            2 => true,
            _ => personalization("AppsUseLightTheme") == Some(0),
        };
        let state = (hwnd, allowed, dark);
        if self.mica == Some(state) {
            return;
        }
        self.mica = Some(state);
        let backdrop: i32 = if allowed { 2 } else { 1 };
        let dark_mode: i32 = dark.into();
        let margins = if allowed {
            Margins {
                left: -1,
                right: -1,
                top: -1,
                bottom: -1,
            }
        } else {
            Margins {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
            }
        };
        let applied = unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                20,
                (&dark_mode as *const i32).cast(),
                size_of::<i32>() as u32,
            );
            let backdrop_result = DwmSetWindowAttribute(
                hwnd,
                38,
                (&backdrop as *const i32).cast(),
                size_of::<i32>() as u32,
            );
            let frame_result = DwmExtendFrameIntoClientArea(hwnd, &margins);
            allowed && backdrop_result >= 0 && frame_result >= 0
        };
        if allowed && !applied {
            let none: i32 = 1;
            let margins = Margins {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
            };
            unsafe {
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    38,
                    (&none as *const i32).cast(),
                    size_of::<i32>() as u32,
                );
                let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
            }
        }
        ui.set_mica_enabled(applied);
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
    fn mica_requires_transparency_and_confirmed_non_high_contrast() {
        let normal = SystemContrastColors::default();
        let high_contrast = SystemContrastColors {
            active: true,
            ..normal.clone()
        };
        assert!(mica_allowed(Some(&normal), Some(1)));
        assert!(!mica_allowed(Some(&normal), Some(0)));
        assert!(!mica_allowed(Some(&normal), None));
        assert!(!mica_allowed(None, Some(1)));
        assert!(!mica_allowed(Some(&high_contrast), Some(1)));
    }
}
