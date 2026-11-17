use std::{
    cell::Cell,
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use serde_json::json;
use slint::{
    ComponentHandle, LogicalPosition, LogicalSize, Model, ModelRc, SharedString,
    StandardListViewItem, VecModel, Window,
    platform::{
        Key, Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent, set_platform,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};

use super::{
    AboutWindow, Action, Config, Conflict, Desktop, Filtered, History, MonitorDiagnosticsWindow,
    MonitorWindow, Palette, Query, Reader, RequestCounts, RequestRow, RetryPolicy, SaveRequest,
    SecurityState, Sha256, State, Store, SystemContrastColors, fill_settings, finish_save,
    monitor_snapshot, show_monitor_diagnostics,
};
use crate::monitor::{Record, Status};
use crate::{connection::RetryableFailure, settings::WorkspaceSettings};
use sha2::Digest;

struct SoftwarePlatform;

thread_local! {
    static NATIVE_BOUNDS: std::cell::RefCell<Vec<(i32, i32, i32, i32)>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(super) fn observe_native_bounds(
    window: &slint::Window,
    event: &slint::winit_030::winit::event::WindowEvent,
) {
    if matches!(
        event,
        slint::winit_030::winit::event::WindowEvent::RedrawRequested
    ) && let Some(handle) = super::hwnd(window)
    {
        let mut rect = windows_sys::Win32::Foundation::RECT::default();
        if unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(handle as _, &mut rect)
        } != 0
        {
            NATIVE_BOUNDS.with_borrow_mut(|frames| {
                frames.push((rect.left, rect.top, rect.right, rect.bottom))
            });
        }
    }
}

#[test]
#[ignore = "native isolated window placement; no settings or connection"]
fn native_first_show_keeps_bounds_and_restores_user_position() {
    use windows_sys::Win32::{Foundation::RECT, UI::WindowsAndMessaging::GetWindowRect};
    fn bounds(window: &slint::Window) -> Option<(i32, i32, i32, i32)> {
        let handle = super::hwnd(window)?;
        let mut rect = RECT::default();
        (unsafe { GetWindowRect(handle as _, &mut rect) } != 0).then_some((
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
        ))
    }
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let ui = Desktop::new().unwrap();
    super::prepare_native_window(
        ui.window(),
        (ui.get_initial_width(), ui.get_initial_height()),
        None,
        |_| {},
    );
    assert!(!ui.window().is_visible());
    NATIVE_BOUNDS.with_borrow_mut(Vec::clear);
    ui.show().unwrap();
    let weak = ui.as_weak();
    let restored = Rc::new(Cell::new(false));
    let result = restored.clone();
    slint::Timer::single_shot(Duration::from_millis(350), move || {
        let ui = weak.upgrade().unwrap();
        let current = bounds(ui.window()).unwrap();
        NATIVE_BOUNDS.with_borrow(|frames| {
            assert!(!frames.is_empty());
            assert!(
                frames.iter().all(|rect| *rect == current),
                "bounds changed after first frame"
            );
        });
        ui.window()
            .set_position(slint::PhysicalPosition::new(current.0 + 24, current.1 + 16));
        let moved = bounds(ui.window()).unwrap();
        ui.hide().unwrap();
        super::show_and_activate(ui.window());
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(350), move || {
            let ui = weak.upgrade().unwrap();
            assert_eq!(bounds(ui.window()).unwrap(), moved);
            result.set(true);
            ui.hide().unwrap();
            slint::quit_event_loop().unwrap();
        });
    });
    slint::run_event_loop_until_quit().unwrap();
    assert!(restored.get());
}

impl Platform for SoftwarePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
    }
}

#[test]
#[ignore = "requires an interactive Windows session and the native Slint backend"]
fn native_main_window_maximizes_and_restores() {
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            IsWindowVisible, IsZoomed, PostMessageW, SC_MAXIMIZE, SC_RESTORE, WM_SYSCOMMAND,
        },
    };

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
        .unwrap();
    let ui = Desktop::new().unwrap();
    super::prepare_native_window(
        ui.window(),
        (ui.get_initial_width(), ui.get_initial_height()),
        None,
        |_| {},
    );
    ui.show().unwrap();

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum State {
        AwaitingWindow,
        Maximizing,
        Restoring,
        Restored,
        MessageFailed(u32),
    }

    let state = Rc::new(Cell::new(State::AwaitingWindow));
    let observed = state.clone();
    let weak = ui.as_weak();
    let deadline = Instant::now() + Duration::from_secs(8);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(20),
        move || {
            if Instant::now() >= deadline {
                slint::quit_event_loop().unwrap();
                return;
            }
            let ui = weak.upgrade().unwrap();
            let window = ui.window();
            let Some(hwnd) = super::hwnd(window).map(|handle| handle as HWND) else {
                return;
            };
            if unsafe { IsWindowVisible(hwnd) } == 0 {
                return;
            }
            let maximized = unsafe { IsZoomed(hwnd) } != 0;
            let (command, next) = match (observed.get(), maximized, window.is_maximized()) {
                (State::AwaitingWindow, false, false) => (SC_MAXIMIZE, State::Maximizing),
                (State::Maximizing, true, true) => (SC_RESTORE, State::Restoring),
                (State::Restoring, false, false) => {
                    observed.set(State::Restored);
                    slint::quit_event_loop().unwrap();
                    return;
                }
                _ => return,
            };
            if unsafe { PostMessageW(hwnd, WM_SYSCOMMAND, command as usize, 0) } == 0 {
                observed.set(State::MessageFailed(command));
                slint::quit_event_loop().unwrap();
            } else {
                observed.set(next);
            }
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    timer.stop();
    ui.hide().unwrap();
    assert_eq!(state.get(), State::Restored);
}

#[test]
fn detached_monitor_restores_main_filters_and_selection_without_changing_main() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    let monitor = MonitorWindow::new().unwrap();
    ui.set_record_search("fixture".into());
    ui.set_record_tool("讀取檔案".into());
    ui.set_record_filter(3);
    ui.set_record_sort(1);
    ui.set_record_descending(false);
    ui.set_record_selected(2);
    ui.set_record_follow(false);
    ui.set_record_details(true);
    super::restore_monitor_view(&ui, &monitor);
    assert_eq!(monitor.get_record_search(), ui.get_record_search());
    assert_eq!(monitor.get_record_tool(), ui.get_record_tool());
    assert_eq!(monitor.get_record_filter(), 3);
    assert_eq!(monitor.get_record_sort(), 1);
    assert!(!monitor.get_record_descending());
    assert_eq!(monitor.get_record_selected(), 2);
    assert!(!monitor.get_record_follow());
    assert!(!monitor.get_record_details());
    monitor.set_record_search("independent".into());
    assert_eq!(ui.get_record_search(), "fixture");
    assert!(ui.get_record_details());
}

#[test]
fn opening_desktop_connects_with_saved_preference_even_when_login_start_is_enabled() {
    let mut config = Config::default();
    config.auto_connect = true;
    for auto_start in [false, true] {
        config.auto_start = auto_start;
        assert!(super::should_connect_at_start(None, &config));
        assert!(!super::should_connect_at_start(
            Some(Path::new("snapshot.json")),
            &config
        ));
    }
    config.auto_connect = false;
    assert!(!super::should_connect_at_start(None, &config));
}

#[test]
fn folder_selection_toggles_extends_and_removes_one_draft_batch() {
    use crate::settings::RootSettings;

    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    let mut draft = Config::default();
    draft.workspace.roots = (0..5)
        .map(|i| {
            let id = format!("folder_{i}");
            RootSettings {
                path: format!("C:\\fixture\\{id}").into(),
                name: None,
                excluded_names: vec![],
                id,
            }
        })
        .collect();
    draft.workspace.default_root = "folder_0".into();
    let selected = || {
        ui.get_folders()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.selected.then_some(index))
            .collect::<Vec<_>>()
    };
    super::folders(&ui, &draft);
    super::select_folder(&ui, 0, false, false);
    assert_eq!(selected(), [0]);
    assert_eq!(ui.get_single_folder(), 0);
    super::select_folder(&ui, 2, false, true);
    assert_eq!(selected(), [0, 2]);
    assert_eq!(ui.get_single_folder(), -1);
    super::select_folder(&ui, 3, true, true);
    assert_eq!(selected(), [0, 2, 3]);
    super::select_folder(&ui, 4, true, false);
    assert_eq!(selected(), [2, 3, 4]);
    assert_eq!(ui.get_folder_selection_count(), 3);
    super::select_folder(&ui, 0, false, true);
    assert_eq!(selected(), [0, 2, 3, 4]);

    let before = draft.workspace.clone();
    assert!(draft.remove_roots(&[0, 5]).is_err());
    assert!(draft.workspace == before);
    super::remove_selected_folders(&ui, &mut draft).unwrap();
    assert_eq!(draft.workspace.roots.len(), 1);
    assert_eq!(draft.workspace.roots[0].id, "folder_1");
    assert_eq!(draft.workspace.default_root, "folder_1");
    assert!(
        ui.get_folders()
            .row_data(0)
            .unwrap()
            .text
            .starts_with("folder_1  ")
    );
    assert!(selected().is_empty());
    assert_eq!(ui.get_folder_cursor(), -1);
}

#[test]
fn contrast_colors_override_manual_theme_in_all_windows_and_restore_it() {
    use slint::Color;

    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    let monitor = MonitorWindow::new().unwrap();
    let about = AboutWindow::new().unwrap();
    let diagnostics = MonitorDiagnosticsWindow::new().unwrap();
    ui.window().set_size(LogicalSize::new(1000., 750.));
    monitor.window().set_size(LogicalSize::new(960., 640.));
    about.window().set_size(LogicalSize::new(400., 220.));
    ui.set_theme(1);
    monitor.set_theme(1);
    about.set_theme(1);
    ui.set_page(3);
    ui.show().unwrap();
    monitor.show().unwrap();
    about.show().unwrap();
    let light = ui.global::<Palette>().get_background();

    monitor.set_records(ModelRc::new(VecModel::from(vec![RequestRow {
        operation: "讀取檔案".into(),
        arguments: "合成資料".into(),
        result: "請求錯誤".into(),
        capture: "部分擷取".into(),
        failed: true,
        warned: true,
        ..RequestRow::default()
    }])));
    monitor.set_record_selected(0);
    let colors = SystemContrastColors {
        active: true,
        window_color: Color::from_rgb_u8(16, 25, 44),
        window_text: Color::from_rgb_u8(243, 233, 209),
        button_face: Color::from_rgb_u8(30, 44, 61),
        button_text: Color::from_rgb_u8(223, 231, 243),
        highlight: Color::from_rgb_u8(252, 198, 47),
        highlight_text: Color::from_rgb_u8(19, 15, 4),
        gray_text: Color::from_rgb_u8(180, 174, 162),
    };
    super::appearance::apply(&ui, &monitor, &about, &[&diagnostics], colors.clone());
    ui.set_theme(2);
    monitor.set_theme(2);
    about.set_theme(2);
    diagnostics.set_theme(2);
    for palette in [
        ui.global::<Palette>(),
        monitor.global::<Palette>(),
        about.global::<Palette>(),
        diagnostics.global::<Palette>(),
    ] {
        assert_eq!(palette.get_background().color(), colors.window_color);
        assert_eq!(palette.get_foreground().color(), colors.window_text);
        assert_eq!(palette.get_control_background().color(), colors.button_face);
        assert_eq!(palette.get_control_foreground().color(), colors.button_text);
        assert_eq!(palette.get_selection_background().color(), colors.highlight);
        assert_eq!(
            palette.get_selection_foreground().color(),
            colors.highlight_text
        );
    }
    let highlighted_pixels = || {
        monitor
            .window()
            .take_snapshot()
            .unwrap()
            .as_bytes()
            .chunks_exact(4)
            .filter(|pixel| *pixel == [252, 198, 47, 255])
            .count()
    };
    let selected_pixels = highlighted_pixels();
    assert_eq!(monitor.get_record_selected(), 0);
    monitor.set_record_selected(-1);
    assert!(selected_pixels > highlighted_pixels());
    monitor.set_record_selected(0);

    super::appearance::apply(
        &ui,
        &monitor,
        &about,
        &[&diagnostics],
        SystemContrastColors::default(),
    );
    let restored = ui.global::<Palette>().get_background();
    assert_ne!(restored.color(), colors.window_color);
    assert_ne!(restored, light);
    assert_eq!(monitor.global::<Palette>().get_background(), restored);
    assert_eq!(about.global::<Palette>().get_background(), restored);
    assert_eq!(diagnostics.global::<Palette>().get_background(), restored);

    ui.hide().unwrap();
    monitor.hide().unwrap();
    about.hide().unwrap();
}

#[test]
fn saving_keeps_new_edits_and_launch_checks_the_latest_disk_settings() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    let fixture = tempfile::Builder::new()
        .prefix("mcp-settings-view-")
        .tempdir()
        .unwrap();
    let path = fixture.path().join("state");
    let shared = fixture.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let authority = super::security::bind(&ui, path.clone());
    let store = Store { path };
    let mut config = Config::default();
    config.workspace = WorkspaceSettings::single(shared);
    config.tunnel = "tunnel_settings_fixture".into();
    config.icon = 1;
    let saved = store.commit(config.clone(), None, false).unwrap();
    fill_settings(&ui, &config);
    assert_eq!(ui.get_icon_choice(), 1);
    assert_eq!(ui.get_applied_icon(), 0);
    let mut state = State {
        store,
        authority,
        draft: config.clone(),
        saved: None,
        load_failed: false,
        connection: None,
        connection_failed: false,
        saving: None,
        stopping: false,
        cleanup_failed: false,
        restart: false,
        at_login: false,
        retry: RetryPolicy::new(false),
    };

    ui.set_scan_files("還在編輯".into());
    ui.set_api_key("synthetic-new-input".into());
    ui.set_icon_choice(0);
    finish_save(
        &ui,
        &mut state,
        &History::new().unwrap(),
        None,
        SaveRequest {
            action: Action::Start,
            key_hash: Sha256::digest(b"").into(),
            had_changes: true,
        },
        saved,
    )
    .unwrap();
    assert_eq!(ui.get_scan_files(), "還在編輯");
    assert_eq!(ui.get_api_key(), "synthetic-new-input");
    assert_eq!(ui.get_icon_choice(), 0);
    assert_eq!(ui.get_applied_icon(), 1);
    assert!(state.connection.is_none());
    assert!(state.connection_config(&ui).is_err());

    ui.set_api_key("".into());
    fill_settings(&ui, &config);
    assert!(state.connection_config(&ui).is_ok());
    ui.set_auto_start(true);
    let draft = super::collect(&ui, &state).unwrap();
    assert!(draft.auto_start && draft.auto_connect);
    assert!(state.connection_config(&ui).is_err());
    ui.set_auto_start(false);
    config.start_hidden = true;
    let external = state.store.commit(config, state.revision(), false).unwrap();
    assert!(state.connection_config(&ui).err().unwrap().is::<Conflict>());
    assert_ne!(state.revision(), Some(external.revision));

    let history = History::new().unwrap();
    let now = Instant::now();
    for dirty in [false, true] {
        if dirty {
            ui.set_scan_files("修改中的設定".into());
        }
        state.retry = RetryPolicy::new(true);
        state
            .retry
            .schedule(now, RetryableFailure { stable_run: false })
            .unwrap();
        ui.set_retry_pending(true);
        state.advance_retry(&ui, &history, now);
        assert!(ui.get_retry_pending());
        state.advance_retry(&ui, &history, now + Duration::from_secs(6));
        assert!(state.connection.is_none());
        assert!(!state.retry.enabled());
        assert!(!ui.get_retry_pending());
        assert_eq!(ui.get_status(), "自動重試已暫停");
        let expected = if dirty {
            "未儲存的變更"
        } else {
            "SETTINGS_CONFLICT"
        };
        assert!(ui.get_diagnostic().contains(expected));
    }
}

#[test]
fn security_page_separates_draft_save_and_confirmed_event_clear() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let fixture = tempfile::Builder::new()
        .prefix("mcp-security-view-")
        .tempdir()
        .unwrap();
    let path = fixture.path().join("state");
    let ui = Desktop::new().unwrap();
    ui.window().set_size(LogicalSize::new(850., 680.));
    ui.set_compact(true);
    let authority = super::security::bind(&ui, path.clone());
    ui.set_page(5);
    ui.show().unwrap();
    save_fixture(ui.window(), "security-overview.png");
    let view = ui.global::<SecurityState>();
    assert!(view.get_ready());
    assert!(!path.exists());
    click(ui.window(), 158., 128.);
    save_fixture(ui.window(), "security-settings.png");
    view.set_enabled(true);
    view.invoke_test_sample(0);
    assert!(
        view.get_preview().contains("masked"),
        "{}",
        view.get_feedback()
    );
    assert!(!view.get_preview().contains("synthetic_password"));
    assert!(!path.join("security-policy.json").exists());
    view.invoke_save();
    assert!(
        crate::security::policy::Store(path.clone())
            .load()
            .unwrap()
            .policy
            .enabled
    );
    let mode = crate::security::channel::Mode::ReadOnly;
    let descriptor = authority.lock().unwrap().begin(mode).unwrap();
    let peer = crate::security::Runtime::attach(path.clone(), &descriptor, mode).unwrap();
    authority.lock().unwrap().tick();
    super::security::reflect(&ui, &authority);
    assert_eq!(view.get_effective(), "執行層已確認套用政策");
    assert!(!view.get_can_grant());
    drop(peer);
    authority.lock().unwrap().close();
    let audit = crate::security::audit::Audit(path);
    let snapshot = audit.export().unwrap();
    assert_eq!(snapshot["chain"].as_array().unwrap().len(), 2);
    click(ui.window(), 231., 128.);
    assert!(view.get_event_status().contains("2 筆"));
    save_fixture(ui.window(), "security-events.png");
    view.set_clear_confirmation("清除安全事件".into());
    view.invoke_clear_events();
    assert_eq!(
        audit.export().unwrap()["chain"][0]["event"]["prior_head"],
        snapshot["revision"]
    );
    ui.hide().unwrap();
}

#[test]
fn minimized_main_window_is_hidden_only_when_the_tray_is_available() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    ui.show().unwrap();
    ui.window().set_minimized(true);
    super::reflect_tray(&ui, false);
    assert!(ui.window().is_visible());
    super::reflect_tray(&ui, true);
    assert!(!ui.window().is_visible());
    ui.show().unwrap();
    ui.window().set_minimized(false);
    super::reflect_tray(&ui, true);
    assert!(ui.window().is_visible());
    ui.hide().unwrap();
}

fn click(window: &Window, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed { position, button });
    window.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn key(window: &Window, key: impl Into<SharedString>) {
    let text = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn chord(window: &Window, modifier: Key, other: impl Into<SharedString>) {
    let text: SharedString = modifier.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    key(window, other);
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

#[test]
fn fluent_appearance_icon_selection_edits_the_draft() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    ui.window().set_size(LogicalSize::new(1000., 750.));
    ui.set_page(2);
    ui.show().unwrap();
    save_fixture(ui.window(), "appearance-icons.png");
    assert_eq!(ui.get_icon_choice(), 0);
    click(ui.window(), 408., 336.);
    assert_eq!(ui.get_icon_choice(), 1);
    assert_eq!(ui.get_applied_icon(), 0);
    click(ui.window(), 272., 336.);
    assert_eq!(ui.get_icon_choice(), 0);
    key(ui.window(), Key::RightArrow);
    assert_eq!(ui.get_icon_choice(), 1);
    key(ui.window(), Key::LeftArrow);
    assert_eq!(ui.get_icon_choice(), 0);
    ui.hide().unwrap();
}

#[test]
fn fluent_permission_page_selects_a_draft_and_mode_confirmation_lists_approved_roots() {
    use super::{Config, Mode, confirm_mode_change};

    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    ui.window().set_size(LogicalSize::new(1000., 750.));
    ui.set_page(3);
    ui.show().unwrap();
    save_fixture(ui.window(), "permission-mode.png");
    assert_eq!(ui.get_access_mode(), 0);
    click(ui.window(), 272., 230.);
    assert_eq!(ui.get_access_mode(), 1);
    click(ui.window(), 272., 270.);
    assert_eq!(ui.get_access_mode(), 2);
    click(ui.window(), 272., 190.);
    assert_eq!(ui.get_access_mode(), 0);
    ui.hide().unwrap();

    let mut draft = Config::default();
    draft.workspace =
        crate::settings::WorkspaceSettings::single(r"C:\fixture\approved root".into());
    assert!(confirm_mode_change(
        &draft,
        Mode::DeveloperControl,
        |_| panic!("read-only does not need confirmation")
    ));
    draft.access_mode = Mode::DeveloperControl;
    assert!(!confirm_mode_change(&draft, Mode::ReadOnly, |message| {
        assert!(message.contains(r"C:\fixture\approved root"));
        assert!(message.contains(".git"));
        assert!(message.contains("重新連線"));
        false
    }));
    assert!(confirm_mode_change(&draft, Mode::ReadOnly, |_| true));
    assert!(confirm_mode_change(
        &draft,
        Mode::DeveloperControl,
        |_| panic!("unchanged mode does not need confirmation")
    ));
    draft.access_mode = Mode::HostControl;
    assert!(!confirm_mode_change(
        &draft,
        Mode::DeveloperControl,
        |message| {
            assert!(message.contains("憑證"));
            assert!(message.contains("停止舊連線"));
            false
        }
    ));
}

fn save_fixture(window: &Window, name: &str) {
    if let Some(path) = std::env::var_os("MCP_LOCAL_RUST_UI_FIXTURE_DIR") {
        let image = window.take_snapshot().unwrap();
        image::save_buffer(
            Path::new(&path).join(name),
            image.as_bytes(),
            image.width(),
            image.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}

#[test]
fn six_pages_render_in_both_themes_at_three_scale_factors() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();
    fill_settings(&ui, &Config::default());
    ui.show().unwrap();
    for scale in [1.0_f32, 1.5, 2.0] {
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
        ui.window().set_size(LogicalSize::new(1100., 760.));
        assert_eq!(ui.window().scale_factor(), scale);
        for page in 0..6 {
            ui.set_page(page);
            assert_eq!(ui.get_page(), page);
            let mut light = None;
            for theme in [1, 2] {
                ui.set_theme(theme);
                ui.window().take_snapshot().unwrap();
                std::thread::sleep(Duration::from_millis(230));
                slint::platform::update_timers_and_animations();
                let snapshot = ui.window().take_snapshot().unwrap();
                assert_eq!(snapshot.width(), (1100. * scale) as u32);
                assert_eq!(snapshot.height(), (760. * scale) as u32);
                if let Some(light) = light.take() {
                    assert_ne!(snapshot.as_bytes(), light);
                } else {
                    light = Some(snapshot.as_bytes().to_vec());
                }
                save_fixture(
                    ui.window(),
                    &format!(
                        "page-{}-theme-{theme}-scale-{}.png",
                        page + 1,
                        (scale * 100.) as u32
                    ),
                );
            }
        }
    }
    ui.hide().unwrap();
}

#[test]
fn monitor_totals_use_retained_results_and_unknown_durations_sort_last() {
    let mut history = History::new().unwrap();
    let record = |id: &str, tool: &str, status, elapsed_ms| Record {
        id: id.into(),
        time: id.into(),
        local_time: String::new(),
        clock: "12:34:56".into(),
        tool: tool.into(),
        access_mode: Some(crate::security::channel::Mode::ReadOnly),
        status,
        elapsed_ms,
        request_notes: vec![],
        response_notes: vec![],
        request: None,
        response: None,
    };
    history.apply(1, 1, record("first", "read_file", Status::Running, None));
    history.apply(
        2,
        1,
        record("second", "read_file", Status::Success, Some(6.0)),
    );
    history.apply(3, 1, record("third", "read_file", Status::Error, Some(1.0)));
    history.apply(
        4,
        1,
        record("fourth", "hash_files", Status::TransportError, Some(2.0)),
    );
    history.apply(
        5,
        1,
        record("fifth", "hash_files", Status::Incomplete, None),
    );
    history.apply(
        6,
        1,
        record("first", "read_file", Status::Success, Some(3.0)),
    );
    history.apply(7, 1, record("sixth", "read_file", Status::Running, None));

    assert_eq!(
        history.counts(),
        RequestCounts {
            retained: 6,
            succeeded: 2,
            failed: 2
        }
    );
    assert_eq!(history.link_status(false, false), "未連線");
    assert_eq!(history.link_status(true, false), "等待接入");
    assert_eq!(history.link_status(false, true), "監控異常");
    let mut filtered = Filtered::default();
    let mut query = Query {
        search: String::new(),
        tool: crate::monitor::presentation::operation("read_file").into(),
        status: 0,
        follow: true,
        tab: 0,
        selected: -1,
        sort: 1,
        descending: false,
    };
    filtered.rebuild(&history, &query);
    let ids = |state: &Filtered| {
        state
            .rows
            .iter()
            .map(|row| row.key.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&filtered), ["third", "first", "second", "sixth"]);
    query.descending = true;
    filtered.rebuild(&history, &query);
    assert_eq!(ids(&filtered), ["second", "first", "third", "sixth"]);
    assert_eq!(history.counts().retained, 6);
    let mut selected = history.get("first").unwrap().record.clone();
    selected.request = Some(json!({"content":"private-fixture-payload"}));
    selected.request_notes = vec!["部分內容已截斷。".into()];
    let mut other = history.get("second").unwrap().record.clone();
    other.response_notes = vec!["nonselected-fixture-note".into()];
    history.apply(8, 1, selected.clone());
    history.apply(9, 1, other);
    let snapshot = monitor_snapshot(
        &history,
        filtered.rows.len(),
        Some(&selected),
        "監控中",
        Some(crate::security::channel::Mode::DeveloperControl),
        1.5,
    );
    assert_eq!(snapshot["records"], 6);
    assert_eq!(snapshot["visible"], 4);
    assert_eq!(snapshot["selected_id"], "first");
    assert_eq!(snapshot["selected_access_mode"], "read_only");
    assert_eq!(snapshot["backend"]["access_mode"], "developer_control");
    assert_eq!(snapshot["request_notes"][0], "部分內容已截斷。");
    assert_eq!(snapshot["partial_records"], 6);
    assert_eq!(snapshot["elapsed_unknown"], 2);
    assert_eq!(snapshot["dpi_scale"], 1.5);
    assert!(!snapshot.to_string().contains("private-fixture-payload"));
    assert!(!snapshot.to_string().contains("nonselected-fixture-note"));
    history.clear().unwrap();
    assert_eq!(history.counts(), RequestCounts::default());
}

#[test]
fn tray_recovery_clears_only_its_own_warning() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = Desktop::new().unwrap();

    super::update_tray_status(&ui, false);
    assert!(!ui.get_tray_ready());
    assert_eq!(ui.get_diagnostic().as_str(), super::TRAY_UNAVAILABLE);

    super::reflect_tray(&ui, true);
    assert!(ui.get_tray_ready());
    assert!(ui.get_diagnostic().is_empty());

    ui.set_diagnostic("其他診斷".into());
    super::reflect_tray(&ui, false);
    assert!(!ui.get_tray_ready());
    super::reflect_tray(&ui, true);
    assert!(ui.get_tray_ready());
    assert_eq!(ui.get_diagnostic().as_str(), "其他診斷");
}

#[test]
fn monitor_advanced_window_is_reused_and_refreshes_after_close() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let monitor = MonitorWindow::new().unwrap();
    let dialog = MonitorDiagnosticsWindow::new().unwrap();
    monitor.window().set_size(LogicalSize::new(850., 580.));
    dialog.window().set_size(LogicalSize::new(610., 560.));
    monitor.set_theme(1);
    monitor.set_record_capture_issues(true);
    monitor.show().unwrap();
    let weak_dialog = dialog.as_weak();
    let weak_monitor = monitor.as_weak();
    let opens = Rc::new(Cell::new(0));
    let calls = opens.clone();
    monitor.on_record_advanced(move || {
        let dialog = weak_dialog.upgrade().unwrap();
        let monitor = weak_monitor.upgrade().unwrap();
        show_monitor_diagnostics(&dialog, monitor.window(), || {
            calls.set(calls.get() + 1);
            Ok(format!(
                "{{\n  \"監控診斷\": \"合成資料\",\n  \"讀取次數\": {}\n}}",
                calls.get()
            )
            .into())
        })
        .unwrap();
    });
    click(monitor.window(), 775., 548.);
    assert!(dialog.window().is_visible());
    assert_eq!(opens.get(), 1);
    save_fixture(dialog.window(), "monitor-diagnostics.png");
    monitor.invoke_record_advanced();
    assert_eq!(opens.get(), 1);
    dialog.hide().unwrap();
    monitor.invoke_record_advanced();
    assert!(dialog.window().is_visible());
    assert_eq!(opens.get(), 2);
    assert!(dialog.get_snapshot().contains("\"讀取次數\": 2"));
    dialog.hide().unwrap();
    monitor.hide().unwrap();
}

#[test]
fn fluent_monitor_renders_and_restores_keyboard_selection() {
    set_platform(Box::new(SoftwarePlatform)).unwrap();
    let ui = MonitorWindow::new().unwrap();
    ui.window().set_size(LogicalSize::new(850., 580.));
    ui.set_theme(1);
    ui.set_records(ModelRc::new(VecModel::from(
        (0..3)
            .map(|index| RequestRow {
                key: index.to_string().into(),
                when: "12:34:56".into(),
                full_time: "2026-11-16 12:34:56".into(),
                tool: "read_file".into(),
                operation: "讀取檔案".into(),
                arguments: if index == 1 {
                    format!("{}/綜合測試.txt", "專案資料/測試結果/本地檔案".repeat(6)).into()
                } else {
                    format!("資料夾/測試-{index}.txt").into()
                },
                context: "第 10 行起 · 20 行".into(),
                result: if index == 1 {
                    "請求錯誤"
                } else {
                    "已回應"
                }
                .into(),
                elapsed: "1.25 ms".into(),
                capture: if index == 1 { "部分內容" } else { "—" }.into(),
                warned: index == 1,
                failed: index == 1,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_record_selected(0);
    ui.set_record_link("監控中".into());
    ui.set_record_counts(RequestCounts {
        retained: 12,
        succeeded: 9,
        failed: 2,
    });
    ui.show().unwrap();
    save_fixture(ui.window(), "monitor-list.png");
    let clear_count = Rc::new(Cell::new(0));
    let cleared = clear_count.clone();
    ui.on_record_clear(move || cleared.set(cleared.get() + 1));
    click(ui.window(), 790., 155.);
    save_fixture(ui.window(), "monitor-more-menu.png");
    click(ui.window(), 630., 192.);
    assert_eq!(clear_count.get(), 1);
    click(ui.window(), 100., 272.);
    assert!(!ui.get_record_follow());
    assert_eq!(ui.get_record_selected(), 0);
    assert!(!ui.get_record_details());
    key(ui.window(), Key::DownArrow);
    assert_eq!(ui.get_record_selected(), 1);
    key(ui.window(), Key::Return);
    assert!(ui.get_record_details());
    ui.window().set_size(LogicalSize::new(650., 500.));
    ui.set_compact(super::compact(ui.window()));
    ui.set_record_header_limit(super::monitor_header_limit(ui.window()));
    assert!(ui.get_compact());
    assert_eq!(ui.get_record_selected(), 1);
    assert!(ui.get_record_details());
    let mut operation = Reader::default();
    operation.set_text("讀取檔案\n已擷取合成樣本的工具參數。".into());
    ui.set_record_document(operation.show(String::new(), false));
    save_fixture(ui.window(), "monitor-json-compact.png");
    ui.window().set_size(LogicalSize::new(960., 640.));
    ui.set_compact(super::compact(ui.window()));
    ui.set_record_header_limit(super::monitor_header_limit(ui.window()));
    assert!(!ui.get_compact());
    assert_eq!(ui.get_record_selected(), 1);
    assert!(ui.get_record_details());
    ui.set_record_tab(1);
    let mut reader = Reader::default();
    let json = json!({"路徑":"合成😀文字 <tag> **", "條件":[1, true, null], "會保留空白":"  合成"});
    reader.set_json(Some(&json));
    ui.set_record_document(reader.show("部分內容已截斷".into(), false));
    let find_count = Rc::new(Cell::new(0));
    let count = find_count.clone();
    let weak = ui.as_weak();
    ui.on_record_find(move |term, direction| {
        assert_eq!(term.as_str(), "合成");
        reader.find(term.as_str(), direction > 0);
        weak.upgrade()
            .unwrap()
            .set_record_document(reader.show("部分內容已截斷".into(), false));
        count.set(count.get() + 1);
    });
    let light = ui.window().take_snapshot().unwrap();
    ui.set_theme(2);
    ui.window().take_snapshot().unwrap();
    std::thread::sleep(Duration::from_millis(230));
    slint::platform::update_timers_and_animations();
    let dark = ui.window().take_snapshot().unwrap();
    assert_ne!(dark.as_bytes(), light.as_bytes());
    save_fixture(ui.window(), "monitor-json-dark.png");
    chord(ui.window(), Key::Control, "f");
    key(ui.window(), "合成");
    key(ui.window(), Key::F3);
    assert_eq!(find_count.get(), 1);
    let document = ui.get_record_document();
    assert_eq!(
        &document.plain.as_str()[document.hit_start as usize..document.hit_end as usize],
        "合成"
    );
    key(ui.window(), Key::Escape);
    chord(ui.window(), Key::Alt, Key::LeftArrow);
    assert!(!ui.get_record_details());
    assert_eq!(ui.get_record_selected(), 1);
    key(ui.window(), Key::UpArrow);
    assert_eq!(ui.get_record_selected(), 0);
    click(ui.window(), 913., 43.);
    assert!(ui.get_record_details());
    click(ui.window(), 913., 43.);
    assert!(!ui.get_record_details());
    assert_eq!(ui.get_record_selected(), 0);
    ui.hide().unwrap();

    let main = Desktop::new().unwrap();
    main.window().set_size(LogicalSize::new(850., 640.));
    main.set_compact(true);
    main.show().unwrap();
    save_fixture(main.window(), "desktop-compact.png");
    click(main.window(), 32., 75.);
    chord(main.window(), Key::Control, Key::Tab);
    assert_eq!(main.get_page(), 1);
    main.window().set_size(LogicalSize::new(850., 850.));
    main.set_extensions(ModelRc::new(VecModel::from(
        ["txt", "md", "yaml", "toml", "ini", "json", "csv"]
            .into_iter()
            .map(StandardListViewItem::from)
            .collect::<Vec<_>>(),
    )));
    save_fixture(main.window(), "desktop-reader.png");
    let edited = Rc::new(Cell::new(None));
    let received = edited.clone();
    main.on_update_name(move |kind, index, value| received.set(Some((kind, index, value))));
    click(main.window(), 128., 478.);
    key(main.window(), "log");
    key(main.window(), Key::Return);
    assert_eq!(edited.take(), Some((0, 0, "log".into())));
    chord(main.window(), Key::Alt, "5");
    assert_eq!(main.get_page(), 4);
    key(main.window(), Key::DownArrow);
    assert_eq!(main.get_page(), 5);
    main.hide().unwrap();
}
