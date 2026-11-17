use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    fs::OpenOptions,
    io::Write,
    path::Path,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use slint::winit_030::{
    EventResult, WinitWindowAccessor,
    winit::{
        event::{ElementState, TouchPhase, WindowEvent},
        platform::windows::WindowExtWindows,
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::Icon,
    },
};
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, StandardListViewItem,
    Timer, TimerMode, VecModel,
};
use windows_sys::Win32::Foundation::{GetLastError, RECT, SetLastError};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GWLP_HWNDPARENT, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, MessageBoxW,
    SetWindowLongPtrW, WS_OVERLAPPEDWINDOW,
};
use zeroize::Zeroizing;

use crate::{
    connection::{Connection, Event},
    desktop_store::{Config, Conflict, Revision, Snapshot, Store},
    monitor::{Bus, Record, Status, presentation},
    native_tray::{Tray, TrayEvent},
    security::channel::Mode,
    settings::{ReaderSettings, fold},
    startup::{DesktopInstance, Status as StartupStatus, Task as StartupTask},
};

slint::include_modules!();

mod appearance;
mod details;
use details::Reader;
mod retry;
mod security;
use retry::RetryPolicy;
#[cfg(test)]
mod tests;

const MIB: usize = 1024 * 1024;
const MAX_RECORDS: usize = 200;
const MAX_HISTORY_BYTES: usize = 32 * MIB;

struct Entry {
    received: u64,
    revision: u64,
    size: usize,
    record: Record,
}

struct History {
    bus: Bus,
    cursor: u64,
    lost: u64,
    evicted: u64,
    enabled: bool,
    records: VecDeque<Entry>,
    bytes: usize,
}

impl History {
    fn new() -> Result<Self> {
        Ok(Self {
            bus: Bus::create()?,
            cursor: 0,
            lost: 0,
            evicted: 0,
            enabled: true,
            records: VecDeque::new(),
            bytes: 0,
        })
    }

    fn poll(&mut self) -> bool {
        let (lost, records) = self.bus.poll(&mut self.cursor);
        let changed = lost > 0 || !records.is_empty();
        self.lost = self.lost.saturating_add(lost);
        for (sequence, bytes, record) in records {
            self.apply(sequence, bytes, record);
        }
        changed
    }

    fn apply(&mut self, sequence: u64, size: usize, record: Record) {
        if let Some(entry) = self
            .records
            .iter_mut()
            .find(|entry| entry.record.id == record.id)
        {
            self.bytes -= entry.size;
            entry.record = record;
            entry.revision = sequence;
            entry.size = size;
        } else {
            self.records.push_back(Entry {
                received: sequence,
                revision: sequence,
                size,
                record,
            });
        }
        self.bytes += size;
        while self.records.len() > MAX_RECORDS || self.bytes > MAX_HISTORY_BYTES {
            let Some(entry) = self.records.pop_front() else {
                break;
            };
            self.bytes -= entry.size;
            self.evicted = self.evicted.saturating_add(1);
        }
    }

    fn clear(&mut self) -> Result<()> {
        self.cursor = self.bus.change(None, true)?;
        self.records.clear();
        self.bytes = 0;
        self.lost = 0;
        self.evicted = 0;
        Ok(())
    }

    fn toggle(&mut self) -> Result<()> {
        self.bus.change(Some(!self.enabled), false)?;
        self.enabled = !self.enabled;
        if !self.enabled {
            self.poll();
            self.retire_pending(Status::Paused, "監控暫停，回應未擷取。");
        }
        Ok(())
    }

    fn retire_pending(&mut self, status: Status, note: &str) -> bool {
        let mut changed = false;
        for entry in self
            .records
            .iter_mut()
            .filter(|entry| entry.record.status == Status::Running)
        {
            entry.record.status = status;
            entry.record.response_notes.push(note.into());
            changed = true;
        }
        changed
    }

    fn get(&self, key: &str) -> Option<&Entry> {
        self.records.iter().find(|entry| entry.record.id == key)
    }

    fn counts(&self) -> RequestCounts {
        let mut counts = RequestCounts {
            retained: self.records.len() as i32,
            ..Default::default()
        };
        for entry in &self.records {
            match entry.record.status {
                Status::Success => counts.succeeded += 1,
                Status::Error | Status::TransportError => counts.failed += 1,
                _ => (),
            }
        }
        counts
    }

    fn partial_records(&self) -> usize {
        self.records
            .iter()
            .filter(|entry| presentation::capture_warning(&entry.record) != "—")
            .count()
    }

    fn link_status(&self, active: bool, failed: bool) -> &'static str {
        if failed {
            return "監控異常";
        }
        if !active {
            return "未連線";
        }
        match self.bus.attached() {
            Ok(true) => "監控中",
            Ok(false) => "等待接入",
            Err(_) => "監控異常",
        }
    }
}

#[derive(Default)]
struct Filtered {
    rows: Vec<RequestRow>,
    tool_options: Vec<String>,
    detail_key: SharedString,
    revision: u64,
    readers: [Reader; 3],
}

struct Query {
    search: String,
    tool: String,
    status: i32,
    follow: bool,
    tab: usize,
    selected: i32,
    sort: i32,
    descending: bool,
}

impl Filtered {
    fn rebuild(&mut self, history: &History, query: &Query) -> i32 {
        let previous = self
            .rows
            .get(query.selected as usize)
            .map(|row| row.key.clone());
        let search = fold(query.search.trim());
        self.tool_options = std::iter::once("全部操作".into())
            .chain(
                history
                    .records
                    .iter()
                    .map(|entry| presentation::operation(&entry.record.tool).to_owned())
                    .chain((!query.tool.is_empty()).then(|| query.tool.clone()))
                    .collect::<BTreeSet<_>>(),
            )
            .collect();
        let status = usize::try_from(query.status - 1)
            .ok()
            .and_then(|index| Status::ALL.get(index));
        let mut entries: Vec<_> = history
            .records
            .iter()
            .filter(|entry| {
                let record = &entry.record;
                let label = presentation::operation(&record.tool);
                if status.is_some_and(|status| *status != record.status)
                    || !query.tool.is_empty() && label != query.tool
                {
                    return false;
                }
                search.is_empty()
                    || fold(label).contains(&search)
                    || fold(&record.tool).contains(&search)
                    || [record.request.as_ref(), record.response.as_ref()]
                        .into_iter()
                        .flatten()
                        .any(|payload| presentation::contains(payload, &search))
                    || record
                        .request_notes
                        .iter()
                        .chain(&record.response_notes)
                        .any(|note| fold(note).contains(&search))
            })
            .collect();
        entries.sort_by(|a, b| {
            let primary = if query.sort == 1 {
                let missing = a
                    .record
                    .elapsed_ms
                    .is_none()
                    .cmp(&b.record.elapsed_ms.is_none());
                if !missing.is_eq() {
                    return missing;
                }
                a.record
                    .elapsed_ms
                    .partial_cmp(&b.record.elapsed_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.record.time.cmp(&b.record.time)
            };
            let order = primary.then(a.received.cmp(&b.received));
            if query.descending {
                order.reverse()
            } else {
                order
            }
        });
        self.rows = entries
            .iter()
            .map(|entry| {
                let record = &entry.record;
                let (target, context) = presentation::row(record);
                let capture = presentation::capture_warning(record);
                RequestRow {
                    key: record.id.as_str().into(),
                    when: record.clock.as_str().into(),
                    full_time: if record.local_time.is_empty() {
                        &record.time
                    } else {
                        &record.local_time
                    }
                    .as_str()
                    .into(),
                    tool: record.tool.as_str().into(),
                    operation: presentation::operation(&record.tool).into(),
                    arguments: target.into(),
                    context: context.into(),
                    result: record.status.label().into(),
                    elapsed: record
                        .elapsed_ms
                        .map_or_else(|| "—".into(), |ms| format!("{ms:.2} ms"))
                        .into(),
                    capture: capture.into(),
                    warned: capture != "—",
                    failed: matches!(record.status, Status::Error | Status::TransportError),
                }
            })
            .collect();
        if query.follow {
            entries
                .iter()
                .enumerate()
                .max_by_key(|(_, entry)| entry.received)
                .map_or(-1, |(index, _)| index as i32)
        } else {
            previous
                .and_then(|key| self.rows.iter().position(|row| row.key == key))
                .map_or(-1, |index| index as i32)
        }
    }

    fn select<'a>(&mut self, history: &'a History, query: &Query) -> Option<&'a Record> {
        let entry = self
            .rows
            .get(query.selected as usize)
            .and_then(|row| history.get(&row.key));
        let Some(entry) = entry else {
            self.detail_key = "".into();
            self.readers = Default::default();
            return None;
        };
        let record = &entry.record;
        if self.detail_key.as_str() != record.id {
            self.readers = Default::default();
            self.detail_key = record.id.as_str().into();
            self.revision = 0;
        }
        if self.revision != entry.revision {
            self.revision = entry.revision;
            self.readers[0].set_text(presentation::detail(record));
            self.readers[1].set_json(record.request.as_ref());
            self.readers[2].set_json(record.response.as_ref());
        }
        Some(record)
    }

    fn document(&mut self, record: Option<&Record>, tab: usize) -> DetailPage {
        let Some(record) = record else {
            return DetailPage::default();
        };
        let (mut notice, missing) = match tab {
            1 => (record.request_notes.join("\n"), record.request.is_none()),
            2 => (record.response_notes.join("\n"), record.response.is_none()),
            _ => (
                record
                    .request_notes
                    .iter()
                    .chain(&record.response_notes)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                false,
            ),
        };
        if missing && notice.is_empty() {
            notice = if tab == 2 && record.status == Status::Running {
                "等待回應".into()
            } else {
                "內容缺失".into()
            };
        }
        self.readers[tab].show(notice, missing)
    }
}

struct State {
    store: Store,
    authority: crate::security::authority::Shared,
    draft: Config,
    saved: Option<Snapshot>,
    load_failed: bool,
    connection: Option<Connection>,
    connection_failed: bool,
    saving: Option<thread::JoinHandle<()>>,
    stopping: bool,
    cleanup_failed: bool,
    restart: bool,
    at_login: bool,
    retry: RetryPolicy,
}

impl State {
    fn revision(&self) -> Option<Revision> {
        self.saved.as_ref().map(|snapshot| snapshot.revision)
    }

    fn has_local_changes(&self, ui: &Desktop) -> bool {
        !ui.get_api_key().is_empty()
            || match (&self.saved, collect(ui, self)) {
                (Some(snapshot), Ok(draft)) => draft != snapshot.config,
                (None, Ok(draft)) => draft != Config::default(),
                (_, Err(_)) => true,
            }
    }

    fn connection_config(&self, ui: &Desktop) -> Result<Config> {
        ensure!(
            !self.has_local_changes(ui),
            "設定有未儲存的變更，請確認後重新啟動連線。"
        );
        let snapshot = self.store.load()?.ok_or(Conflict)?;
        ensure!(Some(snapshot.revision) == self.revision(), Conflict);
        Ok(snapshot.config)
    }

    fn stop(&mut self, ui: &Desktop, restart: bool) {
        if let Some(connection) = &self.connection {
            connection.stop();
            self.restart = restart;
            self.stopping = true;
            ui.set_busy(true);
            ui.set_status("停止中".into());
        }
    }

    fn cancel_retry(&mut self, ui: &Desktop) {
        if self.retry.cancel() {
            ui.set_retry_pending(false);
            ui.set_status("已停止".into());
        }
    }

    fn advance_retry(&mut self, ui: &Desktop, history: &History, now: Instant) {
        if self.saving.is_some() || self.stopping || !self.retry.take_due(now) {
            return;
        }
        ui.set_retry_pending(false);
        if let Err(error) = launch(ui, self, history) {
            self.cancel_retry(ui);
            ui.set_status("自動重試已暫停".into());
            report(ui, error);
            let _ = ui.show();
        }
    }
}

fn list(values: impl IntoIterator<Item = String>) -> ModelRc<StandardListViewItem> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(|text| StandardListViewItem::from(SharedString::from(text)))
            .collect::<Vec<_>>(),
    ))
}

fn folders(ui: &Desktop, draft: &Config) {
    let items: Vec<_> = draft
        .workspace
        .roots
        .iter()
        .map(|entry| FolderRow {
            text: format!(
                "{}  {}",
                entry.name.as_deref().unwrap_or(&entry.id),
                entry.path.display()
            )
            .into(),
            selected: false,
        })
        .collect();
    ui.set_folders(ModelRc::new(VecModel::from(items)));
    ui.set_folder_cursor(-1);
    ui.set_folder_anchor(-1);
    ui.set_folder_selection_count(0);
    ui.set_single_folder(-1);
}

fn update_folder_selection(ui: &Desktop, selected: impl Fn(usize, bool) -> bool) {
    let items = ui.get_folders();
    let (mut count, mut last) = (0, -1);
    for (index, mut row) in items.iter().enumerate() {
        let next = selected(index, row.selected);
        if next {
            count += 1;
            last = index as i32;
        }
        if next != row.selected {
            row.selected = next;
            items.set_row_data(index, row);
        }
    }
    ui.set_folder_selection_count(count);
    ui.set_single_folder(if count == 1 { last } else { -1 });
}

fn select_folder(ui: &Desktop, index: i32, extend: bool, toggle: bool) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    let items = ui.get_folders().row_count();
    if index >= items {
        return;
    }
    let anchor = usize::try_from(ui.get_folder_anchor())
        .ok()
        .filter(|anchor| *anchor < items);
    if !extend || anchor.is_none() {
        ui.set_folder_anchor(index as i32);
    }
    if extend {
        let anchor = anchor.unwrap_or(index);
        let range = anchor.min(index)..=anchor.max(index);
        update_folder_selection(ui, |row, selected| {
            range.contains(&row) || toggle && selected
        });
    } else if toggle {
        update_folder_selection(ui, |row, selected| selected ^ (row == index));
    } else {
        update_folder_selection(ui, |row, _| row == index);
    }
}

fn remove_selected_folders(ui: &Desktop, draft: &mut Config) -> Result<()> {
    let selected: Vec<_> = ui
        .get_folders()
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.selected.then_some(index))
        .collect();
    draft.remove_roots(&selected)?;
    folders(ui, draft);
    Ok(())
}

fn names(ui: &Desktop, settings: &ReaderSettings) {
    ui.set_extensions(list(settings.extensions.clone()));
    ui.set_text_names(list(settings.text_names.clone()));
    ui.set_exclusions(list(settings.excluded_names.clone()));
}

fn numeric(ui: &Desktop, settings: &ReaderSettings) {
    ui.set_file_mib(
        (settings.max_file_bytes as f64 / MIB as f64)
            .to_string()
            .into(),
    );
    ui.set_scan_mib(
        (settings.max_scan_bytes as f64 / MIB as f64)
            .to_string()
            .into(),
    );
    ui.set_scan_files(settings.max_scan_files.to_string().into());
    ui.set_scan_entries(settings.max_scan_entries.to_string().into());
}

fn key_status(ui: &Desktop, config: &Config) {
    ui.set_key_status(
        if config.key_present() {
            "已保存 Windows DPAPI 密文"
        } else {
            "API 金鑰尚未保存"
        }
        .into(),
    );
}

fn fill_settings(ui: &Desktop, draft: &Config) {
    folders(ui, draft);
    names(ui, &draft.reader);
    numeric(ui, &draft.reader);
    ui.set_tunnel(draft.tunnel.as_str().into());
    ui.set_start_hidden(draft.start_hidden);
    ui.set_auto_start(draft.auto_start);
    ui.set_access_mode(match draft.access_mode {
        Mode::ReadOnly => 0,
        Mode::DeveloperControl => 1,
        Mode::HostControl => 2,
    });
    ui.set_icon_choice(draft.icon as i32);
    key_status(ui, draft);
}

fn bytes(text: SharedString) -> Result<usize> {
    let number = text
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("容量必須是數字。"))?
        * MIB as f64;
    ensure!(
        number.is_finite() && number >= 1. && number <= (1024 * MIB) as f64 && number.fract() == 0.,
        "容量超出範圍或不是整數位元組。"
    );
    Ok(number as usize)
}

fn collect(ui: &Desktop, state: &State) -> Result<Config> {
    let mut draft = state.draft.clone();
    draft.access_mode = match ui.get_access_mode() {
        0 => Mode::ReadOnly,
        1 => Mode::DeveloperControl,
        2 => Mode::HostControl,
        _ => anyhow::bail!("權限模式無效。"),
    };
    draft.reader.max_file_bytes = bytes(ui.get_file_mib())?;
    draft.reader.max_scan_bytes = bytes(ui.get_scan_mib())?;
    draft.reader.max_scan_files = ui
        .get_scan_files()
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("掃描檔案上限須為正整數。"))?;
    draft.reader.max_scan_entries = ui
        .get_scan_entries()
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("掃描項目上限須為正整數。"))?;
    draft.reader = draft.reader.normalize()?;
    draft.tunnel = ui.get_tunnel().trim().into();
    draft.icon = ui.get_icon_choice() as usize;
    draft.start_hidden = ui.get_start_hidden();
    if let Some(first) = draft.workspace.roots.first() {
        draft.workspace.default_root = first.id.clone();
    }
    if draft.auto_start != ui.get_auto_start() {
        draft.auto_start = ui.get_auto_start();
        draft.auto_connect = draft.auto_start;
    }
    Ok(draft)
}

fn launch(ui: &Desktop, state: &mut State, history: &History) -> Result<()> {
    ensure!(!state.cleanup_failed, "舊連線停止未確認，無法重新啟動。");
    ensure!(state.connection.is_none(), "連線流程已在執行中。");
    let config = state.connection_config(ui)?;
    let mode = config.access_mode;
    state.connection = Some(Connection::start(
        config,
        history.bus.name().into(),
        state.authority.clone(),
    )?);
    state.connection_failed = false;
    ui.set_connection_mode(
        match mode {
            Mode::DeveloperControl => "開發控制",
            Mode::HostControl => "主機控制",
            Mode::ReadOnly => "唯讀",
        }
        .into(),
    );
    ui.set_active(true);
    ui.set_status("啟動中".into());
    ui.set_diagnostic("".into());
    Ok(())
}

#[derive(Clone, Copy)]
enum Action {
    Save,
    Start,
    Restart,
}

struct SaveRequest {
    action: Action,
    key_hash: [u8; 32],
    had_changes: bool,
}

type SaveResult = (SaveRequest, Result<Snapshot>);

fn confirm_mode_change(draft: &Config, previous: Mode, confirm: impl FnOnce(&str) -> bool) -> bool {
    if draft.access_mode == previous || draft.access_mode == Mode::ReadOnly {
        return true;
    }
    if draft.access_mode == Mode::HostControl {
        return confirm(
            "主機命令以目前未提升權限的 Windows 使用者執行，可存取此使用者可用的檔案、開發工具、網路及憑證。命令不受共享資料夾範圍隔離；檔案 API 仍保留路徑限制。\n\n程序受 Job 資源上限及安全中心授權政策控制。\n\n確認後會儲存此模式。若已連線，會先停止舊連線及工作階段，再重新連線。",
        );
    }
    let roots = draft
        .workspace
        .roots
        .iter()
        .map(|root| root.path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    confirm(&format!(
        "Windows Sandbox VM 可直接讀寫下列資料夾的完整內容，包含 .git、隱藏檔及檔案 API 排除項目。\n\n{roots}\n\n偵測到的開發工具鏈僅唯讀映射。VM 不繼承主機憑證，網路及剪貼簿關閉。\n\n確認後會儲存此模式。若已連線，會先停止舊連線及工作階段，再重新連線。"
    ))
}

fn confirm_warning(ui: &Desktop, title: &str, message: &str) -> bool {
    let title = title.encode_utf16().chain([0]).collect::<Vec<_>>();
    let message = message.encode_utf16().chain([0]).collect::<Vec<_>>();
    ui.window()
        .with_winit_window(|window| {
            let handle = window.window_handle().ok()?;
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                return None;
            };
            Some(unsafe {
                MessageBoxW(
                    handle.hwnd.get() as _,
                    message.as_ptr(),
                    title.as_ptr(),
                    MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                ) == IDYES
            })
        })
        .flatten()
        .unwrap_or(false)
}

fn save(
    ui: &Desktop,
    state: &RefCell<State>,
    sender: &mpsc::Sender<SaveResult>,
    action: Action,
) -> Result<()> {
    ensure!(!ui.get_busy(), "已有設定操作進行中。");
    let key = Zeroizing::new(ui.get_api_key().to_string());
    let (mut draft, previous, path, revision, had_changes, owns_background) = {
        let state = state.borrow();
        ensure!(!state.load_failed, "已保留無法載入的設定，請先修復設定檔。");
        let draft = collect(ui, &state)?;
        let previous = state
            .saved
            .as_ref()
            .map_or(Mode::ReadOnly, |saved| saved.config.access_mode);
        (
            draft,
            previous,
            state.store.path.clone(),
            state.revision(),
            state.has_local_changes(ui),
            state.at_login,
        )
    };
    let allowed = confirm_mode_change(&draft, previous, |message| {
        ui.set_busy(true);
        let choice = confirm_warning(ui, "變更連線權限", message);
        ui.set_busy(false);
        choice
    });
    if !allowed {
        return Ok(());
    }
    let request = SaveRequest {
        action,
        key_hash: Sha256::digest(key.as_bytes()).into(),
        had_changes,
    };
    let sender = sender.clone();
    let worker = thread::Builder::new()
        .name("settings".into())
        .spawn(move || {
            let result = (|| {
                draft.update_key(&key)?;
                Store { path }.commit(draft, revision, owns_background)
            })();
            let _ = sender.send((request, result));
        })?;
    state.borrow_mut().saving = Some(worker);
    ui.set_busy(true);
    ui.set_diagnostic("儲存中".into());
    Ok(())
}

fn report(ui: &Desktop, error: anyhow::Error) {
    ui.set_diagnostic(error.to_string().into());
}

fn apply_icon(ui: &Desktop, tray: Option<&Tray>, icon: usize) -> Result<()> {
    ui.set_applied_icon(icon as i32);
    let window_result = apply_taskbar_icon(ui);
    if let Some(tray) = tray {
        if let Err(error) = tray.set_icon(icon) {
            update_tray_status(ui, tray.ready());
            return Err(error);
        }
    }
    window_result
}

fn apply_taskbar_icon(ui: &Desktop) -> Result<()> {
    ui.window()
        .with_winit_window(|native| {
            let pixels = ui
                .get_applied_icon_image()
                .to_rgba8()
                .ok_or_else(|| anyhow::anyhow!("無法讀取內建視窗圖示。"))?;
            let icon = Icon::from_rgba(pixels.as_bytes().to_vec(), pixels.width(), pixels.height())
                .map_err(|_| anyhow::anyhow!("無法建立 Windows 工作列圖示。"))?;
            native.set_taskbar_icon(Some(icon));
            Ok(())
        })
        .unwrap_or(Ok(()))
}

fn finish_save(
    ui: &Desktop,
    state: &mut State,
    history: &History,
    tray: Option<&Tray>,
    request: SaveRequest,
    snapshot: Snapshot,
) -> Result<()> {
    if !snapshot.config.auto_start || !snapshot.config.auto_connect {
        state.cancel_retry(ui);
    }
    let mode_changed = state
        .connection
        .as_ref()
        .is_some_and(|connection| connection.mode != snapshot.config.access_mode);
    state.draft.retain_key_from(&snapshot.config);
    let key_hash: [u8; 32] = Sha256::digest(ui.get_api_key().as_bytes()).into();
    let key_edited = key_hash != request.key_hash;
    if !key_edited {
        ui.set_api_key("".into());
    }
    key_status(ui, &snapshot.config);
    let icon = snapshot.config.icon;
    state.saved = Some(snapshot);
    let edited = key_edited || state.has_local_changes(ui);
    if !edited {
        state.draft = state.saved.as_ref().unwrap().config.clone();
        names(ui, &state.draft.reader);
    }
    ui.set_diagnostic(if edited {
        "已儲存設定；新編輯的內容仍待儲存。".into()
    } else {
        "已儲存設定".into()
    });
    let restart = !edited && (mode_changed || matches!(request.action, Action::Restart));
    if !edited && matches!(request.action, Action::Start | Action::Restart) {
        let config = &state.saved.as_ref().unwrap().config;
        state
            .retry
            .restart(state.at_login && config.auto_start && config.auto_connect);
        ui.set_retry_pending(false);
    }
    let icon_result = apply_icon(ui, tray, icon);
    if mode_changed || restart && state.connection.is_some() {
        state.stop(ui, restart);
    } else if !edited && matches!(request.action, Action::Start | Action::Restart) {
        launch(ui, state, history)?;
    }
    icon_result
}

fn startup_status(enabled: bool, observation: Result<StartupStatus>) -> &'static str {
    match (enabled, observation) {
        (_, Err(_)) => "登入自啟動：無法查詢",
        (true, Ok(StartupStatus::Missing)) => "登入自啟動：工作遺失",
        (false, Ok(StartupStatus::Missing)) => "登入自啟動：未啟用",
        (true, Ok(StartupStatus::Ready)) => "登入自啟動：已啟用",
        (true, Ok(StartupStatus::Running)) => "登入自啟動：程式仍在執行",
        (_, Ok(_)) => "登入自啟動：工作設定不一致",
    }
}

fn resolve_conflict(
    ui: &Desktop,
    state: &RefCell<State>,
    tray: Option<&Tray>,
    request: SaveRequest,
) -> Result<()> {
    if request.had_changes || state.borrow().has_local_changes(ui) {
        ui.set_busy(true);
        let reload = confirm_warning(
            ui,
            "設定已被其他操作更新",
            "磁碟設定已變更，本次儲存已取消。是否捨棄未儲存的一般設定並重新載入？API 金鑰輸入會保留。",
        );
        ui.set_busy(state.borrow().stopping);
        if !reload {
            return Err(Conflict.into());
        }
    }
    let mut state = state.borrow_mut();
    let saved = state.store.load()?;
    state.draft = saved
        .as_ref()
        .map_or_else(Config::default, |snapshot| snapshot.config.clone());
    state.saved = saved;
    if !state.draft.auto_start || !state.draft.auto_connect {
        state.cancel_retry(ui);
    }
    fill_settings(ui, &state.draft);
    ui.set_diagnostic("已重新載入磁碟設定，請確認後再執行。".into());
    apply_icon(ui, tray, state.draft.icon)
}

const TRAY_UNAVAILABLE: &str = "無法註冊 Windows 系統匣，視窗將保留在工作列。";

fn update_tray_status(ui: &Desktop, ready: bool) {
    ui.set_tray_ready(ready);
    let diagnostic = ui.get_diagnostic();
    if ready && diagnostic.as_str() == TRAY_UNAVAILABLE {
        ui.set_diagnostic(SharedString::default());
    } else if !ready && diagnostic.is_empty() {
        ui.set_diagnostic(TRAY_UNAVAILABLE.into());
    }
}

fn reflect_tray(ui: &Desktop, ready: bool) {
    if ui.get_tray_ready() != ready {
        update_tray_status(ui, ready);
    }
    if !ready {
        return;
    }
    let window = ui.window();
    let minimized = window
        .with_winit_window(|native| {
            native.is_visible() == Some(true) && native.is_minimized() == Some(true)
        })
        .unwrap_or_else(|| window.is_visible() && window.is_minimized());
    if minimized {
        let _ = ui.hide();
    }
}

fn show_and_activate(window: &slint::Window) {
    if window.show().is_ok() {
        window.set_minimized(false);
        window.with_winit_window(|native| native.focus_window());
    }
}

fn compact(window: &slint::Window) -> bool {
    window.size().width as f32 / window.scale_factor() < 920.
}

fn monitor_header_limit(window: &slint::Window) -> f32 {
    (window.size().height as f32 / window.scale_factor() / 4.).clamp(64., 160.)
}

fn update_name(
    ui: &Desktop,
    state: &mut State,
    kind: i32,
    index: i32,
    value: Option<SharedString>,
) {
    if ui.get_busy() {
        return;
    }
    let fields = &mut state.draft.reader;
    let items = match kind {
        0 => &mut fields.extensions,
        1 => &mut fields.text_names,
        2 => &mut fields.excluded_names,
        _ => return,
    };
    match value {
        Some(value) if index < 0 && items.len() < 128 => items.push(value.to_string()),
        Some(value) if (index as usize) < items.len() => items[index as usize] = value.to_string(),
        None if (index as usize) < items.len() => {
            items.remove(index as usize);
        }
        _ => return,
    }
    names(ui, fields);
}

macro_rules! query {
    ($ui:expr) => {
        Query {
            search: $ui.get_record_search().to_string(),
            tool: $ui.get_record_tool().to_string(),
            status: $ui.get_record_filter(),
            follow: $ui.get_record_follow(),
            tab: ($ui.get_record_tab() as usize).min(2),
            selected: $ui.get_record_selected(),
            sort: $ui.get_record_sort(),
            descending: $ui.get_record_descending(),
        }
    };
}

macro_rules! populate {
    ($ui:expr, $model:expr, $history:expr, $rebuild:expr, $edit:expr) => {{
        let mut query = query!($ui);
        let mut model = $model.borrow_mut();
        let history = $history.borrow();
        if $rebuild {
            query.selected = model.rebuild(&history, &query);
            $ui.set_records(ModelRc::new(VecModel::from(model.rows.clone())));
            $ui.set_record_selected(query.selected);
            $ui.set_record_tool_options(ModelRc::new(VecModel::from(
                model
                    .tool_options
                    .iter()
                    .map(|s| SharedString::from(s.as_str()))
                    .collect::<Vec<_>>(),
            )));
            let index = model
                .tool_options
                .iter()
                .position(|value| value == &query.tool)
                .unwrap_or(0);
            $ui.set_record_tool_index(index as i32);
            $ui.set_record_enabled(history.enabled);
            $ui.set_record_counts(history.counts());
            $ui.set_record_capture_issues(
                history.lost > 0 || history.evicted > 0 || history.partial_records() > 0,
            );
            $ui.set_record_health(
                (if history.lost == 0 && history.evicted == 0 {
                    String::new()
                } else {
                    format!("遺失 {} 筆 · 淘汰 {} 筆", history.lost, history.evicted)
                })
                .into(),
            );
        }
        if $ui.get_record_details() {
            let record = model.select(&history, &query);
            let reader = &mut model.readers[query.tab];
            ($edit)(reader);
            if $ui.get_record_tree() && query.tab > 0 {
                $ui.set_record_fields(reader.fields());
            }
            let document = model.document(record, query.tab);
            if $ui.get_record_document() != document {
                $ui.set_record_document(document);
            }
        }
    }};
}

fn export_record(history: &History, model: &Filtered, index: i32) -> Result<()> {
    let Some(row) = model.rows.get(index as usize) else {
        return Ok(());
    };
    let Some(entry) = history.get(&row.key) else {
        return Ok(());
    };
    let data = presentation::export(&entry.record, history.lost, history.evicted);
    let serialized = serde_json::to_string_pretty(&data)?;
    if let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON", &["json"])
        .set_file_name("mcp-request.json")
        .save_file()
    {
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?
            .write_all(format!("{serialized}\n").as_bytes())?;
    }
    Ok(())
}

fn monitor_snapshot(
    history: &History,
    visible: usize,
    selected: Option<&Record>,
    link: &str,
    connection_mode: Option<Mode>,
    dpi_scale: f32,
) -> serde_json::Value {
    let attached = link == "監控中";
    let backend = connection_mode.filter(|_| attached).map(|mode| {
        serde_json::json!({
            "service_version": crate::server::SERVICE_VERSION,
            "contract_version": crate::server::CONTRACT_VERSION,
            "access_mode": mode,
            "runtime": history.bus.runtime_identity()
        })
    });
    serde_json::json!({
        "source": "Rust STDIO",
        "connected": attached,
        "monitor_state": if !history.enabled && matches!(link, "監控中" | "等待接入") { "已暫停" } else { link },
        "records": history.records.len(),
        "visible": visible,
        "selected_id": selected.map(|record| record.id.as_str()),
        "selected_access_mode": selected.and_then(|record| record.access_mode),
        "lost_events": history.lost,
        "evicted_records": history.evicted,
        "partial_records": history.partial_records(),
        "elapsed_unknown": history.records.iter().filter(|entry| entry.record.elapsed_ms.is_none()).count(),
        "scope": "監控歷程累計，非選取紀錄專屬。",
        "request_notes": selected.map(|record| &record.request_notes),
        "response_notes": selected.map(|record| &record.response_notes),
        "capture": "未提供、截斷與省略的內容無法還原。",
        "frontend": { "name": "MCP-Local Rust", "version": env!("CARGO_PKG_VERSION"), "gui": "Slint Fluent", "runtime": crate::build_identity::current() },
        "backend": backend,
        "dpi_scale": dpi_scale,
    })
}

fn hwnd(window: &slint::Window) -> Option<isize> {
    window
        .with_winit_window(|native| match native.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(raw) => Some(raw.hwnd.get()),
            _ => None,
        })
        .flatten()
}

#[link(name = "shcore")]
unsafe extern "system" {
    fn GetDpiForMonitor(monitor: *mut std::ffi::c_void, kind: i32, x: *mut u32, y: *mut u32)
    -> i32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn AdjustWindowRectExForDpi(
        rect: *mut RECT,
        style: u32,
        menu: i32,
        ex_style: u32,
        dpi: u32,
    ) -> i32;
}

fn input_navigation(event: &WindowEvent) -> Option<bool> {
    match event {
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            ..
        }
        | WindowEvent::Touch(slint::winit_030::winit::event::Touch {
            phase: TouchPhase::Started,
            ..
        }) => Some(false),
        WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
            Some(true)
        }
        _ => None,
    }
}

fn prepare_native_window(
    window: &slint::Window,
    initial_size: (f32, f32),
    mut desktop: Option<slint::Weak<Desktop>>,
    input_mode: impl Fn(bool) + 'static,
) {
    // Set the initial bounds before show() creates/maps the native window. A
    // RedrawRequested callback is too late: the user has already seen it.
    let monitor = unsafe { MonitorFromWindow(std::ptr::null_mut(), MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !monitor.is_null() && unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let (mut x_dpi, mut y_dpi) = (96, 96);
        if unsafe { GetDpiForMonitor(monitor, 0, &mut x_dpi, &mut y_dpi) } < 0 {
            x_dpi = 96;
        }
        let dpi = x_dpi.max(96);
        let mut frame = RECT::default();
        if unsafe { AdjustWindowRectExForDpi(&mut frame, WS_OVERLAPPEDWINDOW, 0, 0, dpi) } != 0 {
            let area = info.rcWork;
            let frame_width = frame.right - frame.left;
            let frame_height = frame.bottom - frame.top;
            let width = ((initial_size.0 * dpi as f32 / 96.).round() as i32)
                .min(area.right - area.left - frame_width)
                .max(1);
            let height = ((initial_size.1 * dpi as f32 / 96.).round() as i32)
                .min(area.bottom - area.top - frame_height)
                .max(1);
            window.set_size(slint::PhysicalSize::new(width as u32, height as u32));
            window.set_position(slint::PhysicalPosition::new(
                area.left + (area.right - area.left - width - frame_width) / 2,
                area.top + (area.bottom - area.top - height - frame_height) / 2,
            ));
        }
    }
    window.on_winit_window_event(move |window, event| {
        if let Some(keyboard) = input_navigation(event) {
            input_mode(keyboard);
        }
        #[cfg(test)]
        tests::observe_native_bounds(window, event);
        #[cfg(not(test))]
        let _ = window;
        if matches!(event, WindowEvent::RedrawRequested) {
            if let Some(ui) = desktop.take().and_then(|weak| weak.upgrade())
                && let Err(error) = apply_taskbar_icon(&ui)
            {
                report(&ui, error);
            }
        }
        EventResult::Propagate
    });
}
fn show_monitor_diagnostics(
    dialog: &MonitorDiagnosticsWindow,
    owner: &slint::Window,
    snapshot: impl FnOnce() -> Result<SharedString>,
) -> Result<()> {
    let window = dialog.window();
    if !window.is_visible() {
        dialog.set_snapshot(snapshot()?);
        dialog.show()?;
        if let (Some(parent), Some(child)) = (hwnd(owner), hwnd(window)) {
            let error = unsafe {
                SetLastError(0);
                if SetWindowLongPtrW(child as _, GWLP_HWNDPARENT, parent) == 0 {
                    GetLastError()
                } else {
                    0
                }
            };
            if error != 0 {
                dialog.hide()?;
                anyhow::bail!("無法設定監控診斷的擁有者視窗。");
            }
        }
        dialog.invoke_focus_snapshot();
    }
    window.set_minimized(false);
    window.with_winit_window(|native| native.focus_window());
    Ok(())
}

macro_rules! monitor_events {
    ($ui:expr, $main:expr, $model:expr, $history:expr, $state:expr, $diagnostics:expr) => {{
        let weak = $ui.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_refresh_records(move || {
            if let Some(ui) = weak.upgrade() {
                populate!(ui, model, history, true, |_: &mut Reader| {});
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let main = $main.as_weak();
        let diagnostics = $diagnostics.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        let state = $state.clone();
        $ui.on_record_advanced(move || {
            let (Some(ui), Some(dialog)) = (weak.upgrade(), diagnostics.upgrade()) else {
                return;
            };
            let result = show_monitor_diagnostics(&dialog, ui.window(), || {
                let model = model.borrow();
                let history = history.borrow();
                let selected = model
                    .rows
                    .get(ui.get_record_selected() as usize)
                    .and_then(|row| history.get(&row.key))
                    .map(|entry| &entry.record);
                let mode = state
                    .borrow()
                    .connection
                    .as_ref()
                    .map(|connection| connection.mode);
                let snapshot = monitor_snapshot(
                    &history,
                    model.rows.len(),
                    selected,
                    &ui.get_record_link(),
                    mode,
                    ui.window().scale_factor(),
                );
                Ok(serde_json::to_string_pretty(&snapshot)?.into())
            });
            if let Err(error) = result
                && let Some(main) = main.upgrade()
            {
                report(&main, error);
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_record_page(move |step| {
            if let Some(ui) = weak.upgrade() {
                populate!(ui, model, history, false, |reader: &mut Reader| reader
                    .turn(step));
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_record_find(move |term, direction| {
            if let Some(ui) = weak.upgrade() {
                populate!(ui, model, history, false, |reader: &mut Reader| reader
                    .find(term.as_str(), direction > 0));
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_record_field(move |pointer, action| {
            if let Some(ui) = weak.upgrade() {
                populate!(ui, model, history, false, |reader: &mut Reader| reader
                    .change_field(pointer.as_str(), action));
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let main = $main.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_record_copy(move |whole| {
            let Some(ui) = weak.upgrade() else { return };
            let query = query!(ui);
            let model = model.borrow();
            let history = history.borrow();
            let text = if whole {
                let Some(entry) = model
                    .rows
                    .get(query.selected as usize)
                    .and_then(|row| history.get(&row.key))
                else {
                    return;
                };
                serde_json::to_string_pretty(&presentation::export(
                    &entry.record,
                    history.lost,
                    history.evicted,
                ))
                .unwrap_or_default()
            } else {
                model.readers[query.tab].source().to_owned()
            };
            if text.is_empty() {
                return;
            }
            if let Err(error) =
                arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text))
            {
                if let Some(main) = main.upgrade() {
                    report(&main, error.into());
                }
            }
        });
    }
    {
        let weak = $ui.as_weak();
        let main = $main.as_weak();
        let model = $model.clone();
        let history = $history.clone();
        $ui.on_record_save(move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(error) =
                    export_record(&history.borrow(), &model.borrow(), ui.get_record_selected())
                {
                    if let Some(main) = main.upgrade() {
                        report(&main, error);
                    }
                }
            }
        });
    }};
}

fn should_connect_at_start(import: Option<&Path>, config: &Config) -> bool {
    import.is_none() && config.auto_connect
}

fn restore_monitor_view(ui: &Desktop, monitor: &MonitorWindow) {
    monitor.set_record_search(ui.get_record_search());
    monitor.set_record_tool(ui.get_record_tool());
    monitor.set_record_tool_index(ui.get_record_tool_index());
    monitor.set_record_filter(ui.get_record_filter());
    monitor.set_record_sort(ui.get_record_sort());
    monitor.set_record_descending(ui.get_record_descending());
    monitor.set_record_follow(ui.get_record_follow());
    monitor.set_record_selected(ui.get_record_selected());
    monitor.set_record_details(false);
    monitor.set_record_tab(0);
    monitor.set_record_tree(false);
}

pub fn run(import: Option<&Path>, at_login: bool) -> Result<()> {
    let store = Store::new()?;
    let initial = store.load();
    if at_login {
        let enabled = initial
            .as_ref()
            .map_err(|_| anyhow::anyhow!("無法載入登入連線設定。"))?
            .as_ref()
            .is_some_and(|snapshot| snapshot.config.auto_start);
        if !enabled {
            return Ok(());
        }
        ensure!(
            StartupTask::for_store(&store.path)?.inspect()? == StartupStatus::Running,
            "TASK_MISMATCH：須由已驗證的 Windows 登入工作啟動。"
        );
    }
    let Some(instance) = DesktopInstance::open(&store.path, !at_login)? else {
        ensure!(
            import.is_none(),
            "請先退出目前的 Rust 桌面，再載入設定快照。"
        );
        return Ok(());
    };
    let ui = Desktop::new()?;
    let monitor = MonitorWindow::new()?;
    let weak_monitor_input = monitor.as_weak();
    prepare_native_window(
        monitor.window(),
        (monitor.get_initial_width(), monitor.get_initial_height()),
        None,
        move |keyboard| {
            if let Some(monitor) = weak_monitor_input.upgrade() {
                monitor
                    .global::<WindowsContrast>()
                    .set_keyboard_navigation(keyboard);
            }
        },
    );
    let about = AboutWindow::new()?;
    let main_diagnostics = MonitorDiagnosticsWindow::new()?;
    let monitor_diagnostics = MonitorDiagnosticsWindow::new()?;
    macro_rules! track_dialog_input {
        ($dialog:expr) => {{
            let weak = $dialog.as_weak();
            $dialog.window().on_winit_window_event(move |_, event| {
                if let (Some(keyboard), Some(dialog)) = (input_navigation(event), weak.upgrade()) {
                    dialog
                        .global::<WindowsContrast>()
                        .set_keyboard_navigation(keyboard);
                }
                EventResult::Propagate
            });
        }};
    }
    track_dialog_input!(about);
    track_dialog_input!(main_diagnostics);
    track_dialog_input!(monitor_diagnostics);
    let mut appearance = appearance::Appearance::default();
    appearance.refresh(
        &ui,
        &monitor,
        &about,
        [&main_diagnostics, &monitor_diagnostics],
    );
    let authority = security::bind(&ui, store.path.clone());
    let (saved, load_failed) = match initial {
        Ok(snapshot) => (snapshot, false),
        Err(error) => {
            report(&ui, error);
            (None, true)
        }
    };
    let mut draft = saved
        .as_ref()
        .map_or_else(Config::default, |snapshot| snapshot.config.clone());
    let applied_icon = draft.icon;
    if let Some(path) = import {
        ensure!(!load_failed, "請先修復現有設定。");
        let mut imported = store.import(path)?;
        imported.retain_key_from(&draft);
        draft = imported;
        ui.set_diagnostic("已載入設定快照，請檢查資料夾與通道後儲存。".into());
    }
    let hidden = import.is_none() && (draft.start_hidden || at_login);
    let connect_at_start = should_connect_at_start(import, &draft);
    fill_settings(&ui, &draft);
    ui.set_applied_icon(applied_icon as i32);
    let pending_taskbar_icon = if ui.window().has_winit_window() {
        if let Err(error) = apply_taskbar_icon(&ui) {
            report(&ui, error);
        }
        None
    } else {
        Some(ui.as_weak())
    };
    let weak_ui_input = ui.as_weak();
    prepare_native_window(
        ui.window(),
        (ui.get_initial_width(), ui.get_initial_height()),
        pending_taskbar_icon,
        move |keyboard| {
            if let Some(ui) = weak_ui_input.upgrade() {
                ui.global::<WindowsContrast>()
                    .set_keyboard_navigation(keyboard);
            }
        },
    );
    let tray = Tray::new(applied_icon).ok();
    update_tray_status(&ui, tray.as_ref().is_some_and(Tray::ready));
    let tray = Rc::new(tray);
    let state = Rc::new(RefCell::new(State {
        store,
        authority,
        draft,
        saved,
        load_failed,
        connection: None,
        connection_failed: false,
        saving: None,
        stopping: false,
        cleanup_failed: false,
        restart: false,
        at_login,
        retry: RetryPolicy::new(at_login && connect_at_start),
    }));
    let history = Rc::new(RefCell::new(History::new()?));
    let main_filter = Rc::new(RefCell::new(Filtered::default()));
    let monitor_filter = Rc::new(RefCell::new(Filtered::default()));
    let (save_sender, save_receiver) = mpsc::channel();
    let (startup_sender, startup_receiver) = mpsc::channel();
    {
        let weak = ui.as_weak();
        let path = state.borrow().store.path.clone();
        ui.on_check_startup(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_checking_startup() {
                return;
            }
            ui.set_checking_startup(true);
            let sender = startup_sender.clone();
            let path = path.clone();
            if let Err(error) = thread::Builder::new()
                .name("login-task-status".into())
                .spawn(move || {
                    let _ =
                        sender.send(StartupTask::for_store(&path).and_then(|task| task.inspect()));
                })
            {
                ui.set_checking_startup(false);
                report(&ui, error.into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = save_sender.clone();
        ui.on_save(move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(error) = save(&ui, &state, &sender, Action::Save) {
                    report(&ui, error);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = save_sender.clone();
        ui.on_reconnect(move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(error) = save(&ui, &state, &sender, Action::Restart) {
                    report(&ui, error);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_toggle_connection(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_busy() {
                return;
            }
            let mut current = state.borrow_mut();
            security::reflect(&ui, &current.authority);
            if current.connection.is_some() || current.retry.waiting() {
                current.cancel_retry(&ui);
                current.stop(&ui, false);
            } else if current.cleanup_failed {
                ui.set_diagnostic("舊連線停止未確認，無法重新啟動。".into());
            } else {
                drop(current);
                if let Err(error) = save(&ui, &state, &save_sender, Action::Start) {
                    report(&ui, error);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_folder(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_busy() {
                return;
            }
            if let Some(path) = rfd::FileDialog::new()
                .set_title("選擇授權資料夾")
                .pick_folder()
            {
                let mut state = state.borrow_mut();
                match state.draft.set_root(usize::try_from(index).ok(), path) {
                    Ok(()) => folders(&ui, &state.draft),
                    Err(error) => report(&ui, error),
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_remove_folders(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_busy() {
                    return;
                }
                if let Err(error) = remove_selected_folders(&ui, &mut state.borrow_mut().draft) {
                    report(&ui, error);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_select_folder(move |index, extend, toggle| {
            if let Some(ui) = weak.upgrade()
                && !ui.get_busy()
            {
                select_folder(&ui, index, extend, toggle);
            }
        });
        let weak = ui.as_weak();
        ui.on_select_all_folders(move || {
            if let Some(ui) = weak.upgrade()
                && !ui.get_busy()
            {
                update_folder_selection(&ui, |_, _| true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_reset_numbers(move |maximum| {
            if let Some(ui) = weak.upgrade() {
                if ui.get_busy() {
                    return;
                }
                let mut state = state.borrow_mut();
                if maximum {
                    ui.set_file_mib("64".into());
                    ui.set_scan_mib("1024".into());
                    ui.set_scan_files("100000".into());
                    ui.set_scan_entries("300000".into());
                } else {
                    state.draft.reader = ReaderSettings::default();
                    numeric(&ui, &state.draft.reader);
                    names(&ui, &state.draft.reader);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_update_name(move |kind, index, value| {
            if let Some(ui) = weak.upgrade() {
                update_name(&ui, &mut state.borrow_mut(), kind, index, Some(value));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_remove_name(move |kind, index| {
            if let Some(ui) = weak.upgrade() {
                update_name(&ui, &mut state.borrow_mut(), kind, index, None);
            }
        });
    }
    {
        let weak_monitor = monitor.as_weak();
        let weak_about = about.as_weak();
        let main_diagnostics = main_diagnostics.as_weak();
        let monitor_diagnostics = monitor_diagnostics.as_weak();
        ui.on_theme_changed(move |theme| {
            if let Some(monitor) = weak_monitor.upgrade() {
                monitor.set_theme(theme);
            }
            if let Some(about) = weak_about.upgrade() {
                about.set_theme(theme);
            }
            for dialog in [&main_diagnostics, &monitor_diagnostics]
                .into_iter()
                .filter_map(|weak| weak.upgrade())
            {
                dialog.set_theme(theme);
            }
        });
    }
    {
        let weak = about.as_weak();
        ui.on_about(move || {
            if let Some(about) = weak.upgrade() {
                let _ = about.show();
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_hide_to_tray(move || {
            // The native menu borrows its context until this callback returns.
            let _ = weak.upgrade_in_event_loop(|ui| {
                if ui.get_tray_ready() {
                    let _ = ui.hide();
                }
            });
        });
        let weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if !weak.upgrade().is_some_and(|ui| ui.get_tray_ready()) {
                let _ = slint::quit_event_loop();
            }
            CloseRequestResponse::HideWindow
        });
    }
    {
        let weak = monitor.as_weak();
        let main = ui.as_weak();
        let main_filter = main_filter.clone();
        let monitor_filter = monitor_filter.clone();
        let history = history.clone();
        ui.on_record_detach(move || {
            if let (Some(monitor), Some(ui)) = (weak.upgrade(), main.upgrade()) {
                if !monitor.window().is_visible() {
                    restore_monitor_view(&ui, &monitor);
                    monitor_filter.borrow_mut().rows = main_filter.borrow().rows.clone();
                    populate!(monitor, monitor_filter, history, true, |_: &mut Reader| {});
                }
                show_and_activate(monitor.window());
            }
        });
    }
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    monitor_events!(ui, ui, main_filter, history, state, main_diagnostics);
    monitor_events!(
        monitor,
        ui,
        monitor_filter,
        history,
        state,
        monitor_diagnostics
    );
    for main in [true, false] {
        let weak_ui = ui.as_weak();
        let weak_monitor = monitor.as_weak();
        let history = history.clone();
        let main_filter = main_filter.clone();
        let monitor_filter = monitor_filter.clone();
        let update = move |pause| {
            let result = if pause {
                history.borrow_mut().toggle()
            } else {
                history.borrow_mut().clear()
            };
            if let Some(ui) = weak_ui.upgrade() {
                if let Err(error) = result {
                    report(&ui, error);
                    return;
                }
                populate!(ui, main_filter, history, true, |_: &mut Reader| {});
            }
            if let Some(ui) = weak_monitor.upgrade() {
                populate!(ui, monitor_filter, history, true, |_: &mut Reader| {});
            }
        };
        let clear = update.clone();
        if main {
            ui.on_record_clear(move || clear(false));
            ui.on_record_pause(move || update(true));
        } else {
            monitor.on_record_clear(move || clear(false));
            monitor.on_record_pause(move || update(true));
        }
    }
    populate!(ui, main_filter, history, true, |_: &mut Reader| {});
    populate!(monitor, monitor_filter, history, true, |_: &mut Reader| {});

    let timer = Timer::default();
    {
        let weak = ui.as_weak();
        let weak_monitor = monitor.as_weak();
        let weak_about = about.as_weak();
        let weak_main_diagnostics = main_diagnostics.as_weak();
        let weak_monitor_diagnostics = monitor_diagnostics.as_weak();
        let tray = tray.clone();
        let state = state.clone();
        let history = history.clone();
        let mut appearance_checked = Instant::now();
        let mut mica_checked = Instant::now() - Duration::from_secs(1);
        timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            let Some(ui) = weak.upgrade() else { return };
            if instance.activated() {
                show_and_activate(ui.window());
            }
            if let Some(mode) = instance.requested_mode() {
                show_and_activate(ui.window());
                if ui.get_busy() || state.borrow().has_local_changes(&ui) {
                    ui.set_diagnostic("CLI 模式切換未套用：請先完成或儲存目前的設定編輯。".into());
                } else {
                    ui.set_access_mode(match mode {
                        Mode::ReadOnly => 0,
                        Mode::DeveloperControl => 1,
                        Mode::HostControl => 2,
                    });
                    ui.invoke_save();
                }
            }
            ui.set_compact(compact(ui.window()));
            ui.set_record_header_limit(monitor_header_limit(ui.window()));
            if let Some(monitor) = weak_monitor.upgrade() {
                monitor.set_compact(compact(monitor.window()));
                monitor.set_record_header_limit(monitor_header_limit(monitor.window()));
            }
            let tray = tray.as_ref().as_ref();
            let mut appearance_changed = tray.is_none_or(|tray| !tray.ready())
                && appearance_checked.elapsed() >= Duration::from_secs(1);
            if let Some(tray) = tray {
                reflect_tray(&ui, tray.ready());
                let mode = state
                    .borrow()
                    .saved
                    .as_ref()
                    .map_or(Mode::ReadOnly, |s| s.config.access_mode);
                tray.update_controls(
                    ui.get_busy(),
                    ui.get_active() || ui.get_retry_pending(),
                    match mode {
                        Mode::ReadOnly => 0,
                        Mode::DeveloperControl => 1,
                        Mode::HostControl => 2,
                    },
                );
                while let Ok(event) = tray.events.try_recv() {
                    match event {
                        TrayEvent::Open => show_and_activate(ui.window()),
                        TrayEvent::Start => {
                            if !ui.get_active() && !ui.get_retry_pending() {
                                ui.invoke_toggle_connection();
                            }
                        }
                        TrayEvent::Stop => {
                            if ui.get_active() || ui.get_retry_pending() {
                                ui.invoke_toggle_connection();
                            }
                        }
                        TrayEvent::Mode(mode) => {
                            if !ui.get_busy() {
                                show_and_activate(ui.window());
                                ui.set_access_mode(mode);
                                ui.invoke_reconnect();
                            }
                        }
                        TrayEvent::Lost => {
                            let _ = ui.show();
                            ui.window().set_minimized(false);
                        }
                        TrayEvent::Monitor => {
                            ui.invoke_record_detach();
                        }
                        TrayEvent::AppearanceChanged => {
                            appearance_changed = true;
                        }
                        TrayEvent::Quit => {
                            let _ = slint::quit_event_loop();
                            return;
                        }
                    }
                }
            }
            if appearance_changed {
                if let (
                    Some(monitor),
                    Some(about),
                    Some(main_diagnostics),
                    Some(monitor_diagnostics),
                ) = (
                    weak_monitor.upgrade(),
                    weak_about.upgrade(),
                    weak_main_diagnostics.upgrade(),
                    weak_monitor_diagnostics.upgrade(),
                ) {
                    appearance.refresh(
                        &ui,
                        &monitor,
                        &about,
                        [&main_diagnostics, &monitor_diagnostics],
                    );
                }
                appearance_checked = Instant::now();
            }
            if mica_checked.elapsed() >= Duration::from_secs(1) {
                appearance.refresh_mica(&ui);
                mica_checked = Instant::now();
            }
            while let Ok(observation) = startup_receiver.try_recv() {
                let enabled = state
                    .borrow()
                    .saved
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.config.auto_start);
                ui.set_startup_status(startup_status(enabled, observation).into());
                ui.set_checking_startup(false);
            }
            while let Ok((request, result)) = save_receiver.try_recv() {
                if let Some(worker) = state.borrow_mut().saving.take() {
                    let _ = worker.join();
                }
                ui.set_busy(state.borrow().stopping);
                let outcome = match result {
                    Ok(snapshot) => finish_save(
                        &ui,
                        &mut state.borrow_mut(),
                        &history.borrow(),
                        tray,
                        request,
                        snapshot,
                    ),
                    Err(error) if error.is::<Conflict>() => {
                        resolve_conflict(&ui, &state, tray, request)
                    }
                    Err(error) => Err(error),
                };
                if let Err(error) = outcome {
                    report(&ui, error);
                }
                ui.invoke_check_startup();
            }
            let mut state = state.borrow_mut();
            let mut closed_records = false;
            while let Some(event) = state
                .connection
                .as_ref()
                .and_then(|conn| conn.events.try_recv().ok())
            {
                match event {
                    Event::State(stage) => ui.set_status(stage.into()),
                    Event::Finished {
                        error,
                        cleanup_failed,
                        retry,
                    } => {
                        {
                            let mut history = history.borrow_mut();
                            closed_records |= history.poll();
                            closed_records |= history
                                .retire_pending(Status::Disconnected, "連線已結束，回應未確認。");
                        }
                        state.connection = None;
                        state.connection_failed = error.is_some() || cleanup_failed;
                        state.cleanup_failed |= cleanup_failed;
                        state.stopping = false;
                        ui.set_active(false);
                        ui.set_busy(state.saving.is_some());
                        ui.set_status(if cleanup_failed {
                            "停止未確認".into()
                        } else {
                            "已停止".into()
                        });
                        let failed = error.is_some();
                        ui.set_diagnostic(error.unwrap_or_default().into());
                        let restart = std::mem::take(&mut state.restart);
                        if cleanup_failed {
                            if state.retry.enabled() {
                                let _ = ui.show();
                            }
                            state.cancel_retry(&ui);
                        } else if restart {
                            if let Err(error) = launch(&ui, &mut state, &history.borrow()) {
                                state.cancel_retry(&ui);
                                report(&ui, error);
                            }
                        } else if let Some(delay) =
                            retry.and_then(|failure| state.retry.schedule(Instant::now(), failure))
                        {
                            ui.set_retry_pending(true);
                            ui.set_status(
                                format!("連線暫時失敗，約 {:.0} 秒後重試", delay.as_secs_f64())
                                    .into(),
                            );
                        } else if failed && state.retry.enabled() {
                            state.cancel_retry(&ui);
                            let _ = ui.show();
                        }
                    }
                }
            }
            state.advance_retry(&ui, &history.borrow(), Instant::now());
            let link: SharedString = history
                .borrow()
                .link_status(state.connection.is_some(), state.connection_failed)
                .into();
            if ui.get_record_link() != link {
                ui.set_record_link(link.clone());
                if let Some(monitor) = weak_monitor.upgrade() {
                    monitor.set_record_link(link);
                }
            }
            if history.borrow_mut().poll() || closed_records {
                populate!(ui, main_filter, history, true, |_: &mut Reader| {});
                if let Some(monitor) = weak_monitor.upgrade() {
                    populate!(monitor, monitor_filter, history, true, |_: &mut Reader| {});
                }
            }
        });
    }

    if !hidden || !ui.get_tray_ready() {
        ui.show()?;
    }
    ui.invoke_check_startup();
    if connect_at_start {
        let mut state = state.borrow_mut();
        if let Err(error) = launch(&ui, &mut state, &history.borrow()) {
            state.cancel_retry(&ui);
            report(&ui, error);
            ui.show()?;
        }
    }
    let exit = slint::run_event_loop_until_quit();
    timer.stop();
    if let Some(worker) = state.borrow_mut().saving.take() {
        let _ = worker.join();
    }
    state.borrow_mut().connection.take();
    exit?;
    Ok(())
}
