use std::{
    cell::Cell,
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender},
};

use anyhow::{Result, ensure};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
            Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW,
            DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, GWLP_USERDATA, GetCursorPos,
            GetWindowLongPtrW, HICON, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW,
            RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW,
            TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, UnregisterClassW, WM_APP,
            WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_SYSCOLORCHANGE,
            WM_THEMECHANGED, WNDCLASSW,
        },
    },
};

const CALLBACK: u32 = WM_APP + 5;
const CLASS: &str = "MCP-Local-Rust.Tray";

pub enum TrayEvent {
    Open,
    Monitor,
    Start,
    Stop,
    Mode(i32),
    Quit,
    Lost,
    AppearanceChanged,
}

struct Context {
    icon: Cell<HICON>,
    taskbar_created: u32,
    registered: Cell<bool>,
    controls: Cell<(bool, bool, i32)>,
    send: Sender<TrayEvent>,
}

impl Context {
    fn set_registration(&self, registered: bool) {
        if self.registered.replace(registered) && !registered {
            let _ = self.send.send(TrayEvent::Lost);
        }
    }

    fn register(&self, add_icon: impl FnOnce(HICON) -> bool) {
        self.set_registration(add_icon(self.icon.get()));
    }
}

pub struct Tray {
    hwnd: HWND,
    module: HINSTANCE,
    class: Vec<u16>,
    context: Box<Context>,
    pub events: Receiver<TrayEvent>,
}

impl Tray {
    pub fn new(style: usize) -> Result<Self> {
        let icon = icon(style)?;
        let module = unsafe { GetModuleHandleW(null()) };
        let class = wide(CLASS);
        let atom = unsafe {
            RegisterClassW(&WNDCLASSW {
                hInstance: module,
                lpszClassName: class.as_ptr(),
                lpfnWndProc: Some(window_proc),
                ..Default::default()
            })
        };
        if atom == 0 {
            unsafe {
                DestroyIcon(icon);
            }
            anyhow::bail!("無法建立系統匣視窗。");
        }
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                module,
                null(),
            )
        };
        if hwnd.is_null() {
            unsafe {
                UnregisterClassW(class.as_ptr(), module);
                DestroyIcon(icon);
            }
            anyhow::bail!("無法建立系統匣視窗。");
        }
        let (send, events) = mpsc::channel();
        let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        let context = Box::new(Context {
            icon: Cell::new(icon),
            taskbar_created,
            registered: Cell::new(false),
            controls: Cell::new((false, false, 0)),
            send,
        });
        let tray = Self {
            hwnd,
            module,
            class,
            context,
            events,
        };
        // The boxed context is stable until GWLP_USERDATA is cleared in Drop.
        unsafe {
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                (&*tray.context as *const Context) as isize,
            );
        }
        // Keep the top-level window alive to receive TaskbarCreated even
        // when Explorer has not started at login.
        tray.context
            .register(|icon| unsafe { Shell_NotifyIconW(NIM_ADD, &record(hwnd, icon)) != 0 });
        Ok(tray)
    }

    pub fn ready(&self) -> bool {
        self.context.registered.get()
    }

    pub fn update_controls(&self, busy: bool, active: bool, mode: i32) {
        self.context.controls.set((busy, active, mode));
    }

    pub fn set_icon(&self, style: usize) -> Result<()> {
        let icon = icon(style)?;
        let ready = self.ready();
        let success =
            !ready || unsafe { Shell_NotifyIconW(NIM_MODIFY, &record(self.hwnd, icon)) != 0 };
        unsafe {
            DestroyIcon(self.context.icon.replace(icon));
        }
        if !success {
            self.context.set_registration(false);
            anyhow::bail!("系統匣無法使用。");
        }
        Ok(())
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            Shell_NotifyIconW(NIM_DELETE, &record(self.hwnd, self.context.icon.get()));
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), self.module);
            DestroyIcon(self.context.icon.get());
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn record(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    let mut result = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    let label = wide("MCP-Local");
    result.szTip[..label.len()].copy_from_slice(&label);
    result
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Context;
    if let Some(context) = unsafe { ptr.as_ref() } {
        if matches!(
            message,
            WM_SYSCOLORCHANGE | WM_THEMECHANGED | WM_SETTINGCHANGE
        ) {
            let _ = context.send.send(TrayEvent::AppearanceChanged);
        }
        if message == CALLBACK && wparam == 1 {
            let event = match lparam as u32 {
                WM_LBUTTONUP => Some(TrayEvent::Open),
                WM_RBUTTONUP => popup(hwnd, context.controls.get()),
                _ => None,
            };
            if let Some(event) = event {
                let _ = context.send.send(event);
            }
            return 0;
        }
        if context.taskbar_created != 0 && message == context.taskbar_created {
            context
                .register(|icon| unsafe { Shell_NotifyIconW(NIM_ADD, &record(hwnd, icon)) != 0 });
            return 0;
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn popup(hwnd: HWND, (busy, active, mode): (bool, bool, i32)) -> Option<TrayEvent> {
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    let mut position = POINT::default();
    let item = unsafe {
        AppendMenuW(menu, MF_STRING, 1, wide("開啟 MCP-Local").as_ptr());
        AppendMenuW(menu, MF_STRING, 2, wide("請求監控").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(
            menu,
            MF_STRING | if busy || active { MF_GRAYED } else { 0 },
            4,
            wide("啟動連線").as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING | if busy || !active { MF_GRAYED } else { 0 },
            5,
            wide("停止連線").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        for (index, label) in ["唯讀", "開發控制", "主機控制"].iter().enumerate() {
            let flags = MF_STRING
                | if busy { MF_GRAYED } else { 0 }
                | if mode == index as i32 { MF_CHECKED } else { 0 };
            AppendMenuW(menu, flags, 6 + index, wide(label).as_ptr());
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, 3, wide("退出").as_ptr());
        SetForegroundWindow(hwnd);
        let item = if GetCursorPos(&mut position) != 0 {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                position.x,
                position.y,
                0,
                hwnd,
                null(),
            )
        } else {
            0
        };
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        item
    };
    match item {
        1 => Some(TrayEvent::Open),
        2 => Some(TrayEvent::Monitor),
        3 => Some(TrayEvent::Quit),
        4 => Some(TrayEvent::Start),
        5 => Some(TrayEvent::Stop),
        6..=8 => Some(TrayEvent::Mode(item - 6)),
        _ => None,
    }
}

fn icon(style: usize) -> Result<HICON> {
    let source: &[u8] = if style == 1 {
        include_bytes!("../assets/folder-link.ico")
    } else {
        include_bytes!("../assets/classic.ico")
    };
    let count = u16::from_le_bytes(source[4..6].try_into().unwrap()) as usize;
    let end = 6 + 16 * count;
    ensure!(end <= source.len(), "內建圖示目錄無效。");
    let entry = source[6..end]
        .chunks_exact(16)
        .min_by_key(|entry| {
            let width = if entry[0] == 0 {
                256
            } else {
                usize::from(entry[0])
            };
            (width.abs_diff(32), 255 - entry[6])
        })
        .ok_or_else(|| anyhow::anyhow!("缺少內建圖示。"))?;
    let size = u32::from_le_bytes(entry[8..12].try_into().unwrap());
    let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
    let image = source
        .get(offset..offset + size as usize)
        .ok_or_else(|| anyhow::anyhow!("內建圖示無效。"))?;
    let mut aligned = vec![0u32; image.len().div_ceil(4)];
    // CreateIconFromResourceEx requires DWORD-aligned bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            image.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            image.len(),
        );
    }
    let icon =
        unsafe { CreateIconFromResourceEx(aligned.as_ptr().cast(), size, 1, 0x30000, 32, 32, 0) };
    ensure!(!icon.is_null(), "無法讀取內建圖示。");
    Ok(icon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an interactive Windows Explorer notification area"]
    fn native_tray_registers_changes_icon_recovers_and_delivers_open() {
        let tray = Tray::new(0).unwrap();
        assert!(tray.ready(), "Explorer did not register the test tray icon");
        tray.set_icon(1).unwrap();
        assert!(tray.ready());
        tray.set_icon(0).unwrap();
        unsafe {
            assert_ne!(
                Shell_NotifyIconW(NIM_DELETE, &record(tray.hwnd, tray.context.icon.get())),
                0
            );
            // Simulate Explorer's notification only for this test's own window.
            window_proc(tray.hwnd, tray.context.taskbar_created, 0, 0);
            assert!(tray.ready());
            window_proc(tray.hwnd, CALLBACK, 1, WM_LBUTTONUP as LPARAM);
        }
        assert!(matches!(tray.events.try_recv(), Ok(TrayEvent::Open)));
        assert!(tray.events.try_recv().is_err());
        // Tray::drop removes only this test's icon and window.
    }

    #[test]
    fn registration_recovers_after_shell_start_and_reports_later_loss() {
        let (send, events) = mpsc::channel();
        let context = Context {
            icon: Cell::new(null_mut()),
            taskbar_created: 0,
            registered: Cell::new(false),
            controls: Cell::new((false, false, 0)),
            send,
        };

        context.register(|_| false);
        assert!(!context.registered.get());
        assert!(events.try_recv().is_err());

        context.register(|_| true);
        assert!(context.registered.get());
        assert!(events.try_recv().is_err());

        context.register(|_| false);
        context.register(|_| false);
        assert!(!context.registered.get());
        assert!(matches!(events.try_recv(), Ok(TrayEvent::Lost)));
        assert!(events.try_recv().is_err());

        context.register(|_| true);
        assert!(context.registered.get());
        assert!(events.try_recv().is_err());
    }
}
