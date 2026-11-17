use std::{
    env, fs,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr::null,
};

use anyhow::{Context, Result, bail, ensure};
use roxmltree::{Document, Node};
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, RPC_E_CHANGED_MODE},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            TaskScheduler::{
                ITaskFolder, ITaskService, TASK_CREATE, TASK_LOGON_INTERACTIVE_TOKEN,
                TASK_STATE_QUEUED, TASK_STATE_READY, TASK_STATE_RUNNING, TaskScheduler,
            },
            Variant::VARIANT,
        },
    },
    core::BSTR,
};
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject},
    UI::Shell::FOLDERID_LocalAppData,
};

use crate::{filesystem::known_folder, listing::hex, windows_token};

pub const ARGUMENT: &str = "--startup";
const NS: &str = "http://schemas.microsoft.com/windows/2004/02/mit/task";
const DESCRIPTION: &str = "MCP-Local Rust sign-in";
const MISMATCH: &str = "TASK_MISMATCH：同名登入工作設定不一致，已保留。";

pub struct DesktopInstance(OwnedHandle, Vec<OwnedHandle>);

fn instance_name(state: &Path) -> Result<String> {
    let parent = state.parent().context("無法確認桌面設定目錄。")?;
    let parent = fs::canonicalize(parent)?;
    let digest = Sha256::digest(parent.to_string_lossy().to_lowercase());
    Ok(format!(
        r"Local\MCP-Local-Rust.Desktop.{}.{}",
        windows_token::current_sid()?,
        hex(&digest)
    ))
}

impl DesktopInstance {
    pub fn open(state: &Path, activation: bool) -> Result<Option<Self>> {
        let base = instance_name(state)?;
        let name = base.clone();
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let raw = unsafe { CreateEventW(null(), 0, 0, name.as_ptr()) };
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        ensure!(!raw.is_null(), "無法建立桌面執行鎖定。");
        let mut event = Self(unsafe { OwnedHandle::from_raw_handle(raw) }, Vec::new());
        if exists {
            if activation {
                ensure!(
                    unsafe { SetEvent(event.0.as_raw_handle()) } != 0,
                    "無法開啟執行中的桌面視窗。"
                );
            }
            Ok(None)
        } else {
            for mode in 0..3 {
                let name: Vec<u16> = format!("{base}.Mode{mode}")
                    .encode_utf16()
                    .chain([0])
                    .collect();
                let raw = unsafe { CreateEventW(null(), 0, 0, name.as_ptr()) };
                ensure!(!raw.is_null(), "無法建立本機模式控制事件。");
                event.1.push(unsafe { OwnedHandle::from_raw_handle(raw) });
            }
            Ok(Some(event))
        }
    }

    pub fn activated(&self) -> bool {
        unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }

    pub fn requested_mode(&self) -> Option<crate::security::channel::Mode> {
        use crate::security::channel::Mode;
        self.1.iter().enumerate().find_map(|(i, handle)| {
            (unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } == WAIT_OBJECT_0)
                .then_some([Mode::ReadOnly, Mode::DeveloperControl, Mode::HostControl][i])
        })
    }

    pub fn request_mode(state: &Path, mode: crate::security::channel::Mode) -> Result<()> {
        use crate::security::channel::Mode;
        let index = match mode {
            Mode::ReadOnly => 0,
            Mode::DeveloperControl => 1,
            Mode::HostControl => 2,
        };
        let name: Vec<u16> = format!("{}.Mode{index}", instance_name(state)?)
            .encode_utf16()
            .chain([0])
            .collect();
        // Only signal an existing owner; never create a queued command for a future desktop.
        let raw = unsafe { OpenEventW(2, 0, name.as_ptr()) };
        ensure!(
            !raw.is_null(),
            "MODE_SWITCH_UNVERIFIED：無法確認執行中的 Rust 桌面。"
        );
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        ensure!(
            unsafe { SetEvent(handle.as_raw_handle()) } != 0,
            "MODE_SWITCH_UNVERIFIED"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Missing,
    Ready,
    Running,
    Mismatch,
}

impl Status {
    fn registered(self) -> bool {
        matches!(self, Self::Ready | Self::Running)
    }
}

struct Definition {
    sid: String,
    executable: String,
    directory: String,
}

const SETTINGS: [(&str, &str); 10] = [
    ("MultipleInstancesPolicy", "IgnoreNew"),
    ("DisallowStartIfOnBatteries", "false"),
    ("StopIfGoingOnBatteries", "false"),
    ("AllowStartOnDemand", "true"),
    ("Enabled", "true"),
    ("Hidden", "true"),
    ("ExecutionTimeLimit", "PT0S"),
    ("StartWhenAvailable", "false"),
    ("RunOnlyIfIdle", "false"),
    ("RunOnlyIfNetworkAvailable", "false"),
];

impl Definition {
    fn name(&self) -> BSTR {
        BSTR::from(format!("MCP-Local Rust Auto Connect {}", self.sid))
    }

    fn xml(&self) -> String {
        let escaped = |text: &str| {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let settings: String = SETTINGS
            .iter()
            .map(|(name, value)| format!("<{name}>{value}</{name}>"))
            .collect();
        let Self {
            sid,
            executable,
            directory,
        } = self;
        format!(
            concat!(
                "<Task xmlns=\"{ns}\" version=\"1.2\">",
                "<RegistrationInfo><Description>{description}</Description></RegistrationInfo>",
                "<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>",
                "<Principals><Principal id=\"Author\"><UserId>{sid}</UserId>",
                "<LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>",
                "<Settings>{settings}<RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>",
                "<Actions Context=\"Author\"><Exec><Command>{executable}</Command><Arguments>{argument}</Arguments>",
                "<WorkingDirectory>{directory}</WorkingDirectory></Exec></Actions></Task>"
            ),
            ns = NS,
            description = DESCRIPTION,
            sid = sid,
            settings = settings,
            executable = escaped(executable),
            argument = ARGUMENT,
            directory = escaped(directory),
        )
    }

    fn matches(&self, xml: &str, account: &str) -> bool {
        if xml.len() > 64 * 1024 {
            return false;
        }
        let Ok(document) = Document::parse(xml) else {
            return false;
        };
        let root = document.root_element();
        let Some((trigger, principal, actions, settings)) = (|| {
            if !root.has_tag_name((NS, "Task")) {
                return None;
            }
            let info = child(root, "RegistrationInfo")?;
            if value(info, "Description") != Some(DESCRIPTION) {
                return None;
            }
            Some((
                single(root, "Triggers", "LogonTrigger")?,
                single(root, "Principals", "Principal")?,
                child(root, "Actions")?,
                child(root, "Settings")?,
            ))
        })() else {
            return false;
        };
        let user = |node| {
            value(node, "UserId").is_some_and(|id| {
                id.eq_ignore_ascii_case(&self.sid)
                    || !account.is_empty() && id.eq_ignore_ascii_case(account)
            })
        };
        let Some(action) = only(actions, "Exec") else {
            return false;
        };
        let Some(restart) = child(settings, "RestartOnFailure") else {
            return false;
        };
        user(trigger)
            && user(principal)
            && trigger.children().filter(Node::is_element).count()
                == 1 + usize::from(child(trigger, "Enabled").is_some())
            && (child(trigger, "Enabled").is_none() || value(trigger, "Enabled") == Some("true"))
            && principal.children().filter(Node::is_element).count()
                == 2 + usize::from(child(principal, "RunLevel").is_some())
            && value(principal, "LogonType") == Some("InteractiveToken")
            && (child(principal, "RunLevel").is_none()
                || value(principal, "RunLevel") == Some("LeastPrivilege"))
            && principal.attribute("id") == Some("Author")
            && actions.attribute("Context") == Some("Author")
            && action.children().filter(Node::is_element).count() == 3
            && value(action, "Command") == Some(self.executable.as_str())
            && value(action, "Arguments") == Some(ARGUMENT)
            && value(action, "WorkingDirectory") == Some(self.directory.as_str())
            && SETTINGS.iter().all(|(name, expected)| {
                value(settings, name).or_else(|| {
                    if settings
                        .children()
                        .any(|node| node.has_tag_name((NS, *name)))
                    {
                        return None;
                    }
                    match *name {
                        "Enabled" | "AllowStartOnDemand" => Some("true"),
                        "StartWhenAvailable" | "RunOnlyIfIdle" | "RunOnlyIfNetworkAvailable" => {
                            Some("false")
                        }
                        _ => None,
                    }
                }) == Some(*expected)
            })
            && value(restart, "Interval") == Some("PT1M")
            && value(restart, "Count") == Some("3")
    }
}

fn child<'a, 'input>(node: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    let mut elements = node
        .children()
        .filter(|element| element.has_tag_name((NS, tag)));
    let element = elements.next()?;
    elements.next().is_none().then_some(element)
}

fn only<'a, 'input>(node: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    let mut elements = node.children().filter(Node::is_element);
    let element = elements.next()?;
    (element.has_tag_name((NS, tag)) && elements.next().is_none()).then_some(element)
}

fn single<'a, 'input>(
    node: Node<'a, 'input>,
    section: &str,
    tag: &str,
) -> Option<Node<'a, 'input>> {
    only(child(node, section)?, tag)
}

fn value<'a>(node: Node<'a, '_>, tag: &str) -> Option<&'a str> {
    let field = child(node, tag)?;
    if field.children().any(|node| node.is_element()) {
        return None;
    }
    let mut text = field.children().filter(Node::is_text);
    let value = text.next()?.text()?;
    text.next().is_none().then_some(value)
}

struct Apartment(bool);

impl Apartment {
    fn enter() -> Result<Self> {
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if status == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        status.ok()?;
        Ok(Self(true))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() }
        }
    }
}

trait Tasks {
    fn status(&self) -> Result<Status>;
    fn create(&self) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

pub struct Task {
    folder: ITaskFolder,
    definition: Definition,
    account: String,
    _apartment: Apartment,
}

impl Task {
    pub fn for_store(path: &Path) -> Result<Self> {
        const UNAVAILABLE: &str =
            "LOGIN_STARTUP_UNAVAILABLE：登入工作僅使用 Windows 的本機設定目錄。";
        let expected = known_folder(&FOLDERID_LocalAppData)?;
        let parent = path.parent().context(UNAVAILABLE)?;
        ensure!(
            fs::canonicalize(parent)? == fs::canonicalize(expected)?,
            "{UNAVAILABLE}"
        );
        windows_token::require_standard_user("登入工作需要未提升的 Windows 使用者權限。")?;
        let sid = windows_token::current_sid()?;
        let executable = fs::canonicalize(env::current_exe()?)?;
        let executable = executable.to_str().context("無法確認原生程式的路徑。")?;
        let executable = executable.strip_prefix(r"\\?\").unwrap_or(executable);
        ensure!(
            executable.as_bytes().get(1..3) == Some(b":\\")
                && !executable.contains('%')
                && !executable.chars().any(char::is_control),
            "登入工作需要可直接執行的本機安裝路徑。"
        );
        let directory = Path::new(executable)
            .parent()
            .and_then(Path::to_str)
            .context("找不到登入工作的安裝目錄。")?;
        Self::connect(Definition {
            sid,
            executable: executable.to_owned(),
            directory: directory.to_owned(),
        })
        .context("TASK_QUERY_FAILED：無法連線至 Windows 工作排程器。")
    }

    fn connect(definition: Definition) -> Result<Self> {
        let apartment = Apartment::enter()?;
        let empty = VARIANT::default();
        unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            service.Connect(&empty, &empty, &empty, &empty)?;
            let account = format!(
                "{}\\{}",
                service.ConnectedDomain()?,
                service.ConnectedUser()?
            );
            let folder = service.GetFolder(&BSTR::from("\\"))?;
            Ok(Self {
                folder,
                definition,
                account,
                _apartment: apartment,
            })
        }
    }

    pub fn inspect(&self) -> Result<Status> {
        self.status()
            .context("TASK_QUERY_FAILED：無法查詢登入工作。")
    }

    pub fn transaction<T>(
        &self,
        desired: bool,
        owns_background: bool,
        save: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        transaction(self, desired, owns_background, save)
    }
}

impl Tasks for Task {
    fn status(&self) -> Result<Status> {
        let task = match unsafe { self.folder.GetTask(&self.definition.name()) } {
            Ok(task) => task,
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                return Ok(Status::Missing);
            }
            Err(error) => return Err(error.into()),
        };
        let xml = unsafe { task.Xml() }?.to_string();
        if !self.definition.matches(&xml, &self.account) || !unsafe { task.Enabled() }?.as_bool() {
            return Ok(Status::Mismatch);
        }
        let state = unsafe { task.State() }?;
        match state {
            TASK_STATE_RUNNING | TASK_STATE_QUEUED => Ok(Status::Running),
            TASK_STATE_READY => Ok(Status::Ready),
            _ => bail!("TASK_QUERY_FAILED：Windows 尚未確認登入工作的狀態。"),
        }
    }

    fn create(&self) -> Result<()> {
        let empty = VARIANT::default();
        let user = VARIANT::from(BSTR::from(&self.definition.sid));
        unsafe {
            self.folder.RegisterTask(
                &self.definition.name(),
                &BSTR::from(self.definition.xml()),
                TASK_CREATE.0,
                &user,
                &empty,
                TASK_LOGON_INTERACTIVE_TOKEN,
                &empty,
            )?;
        }
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        ensure!(self.inspect()?.registered(), "{MISMATCH}");
        unsafe { self.folder.DeleteTask(&self.definition.name(), 0) }?;
        Ok(())
    }
}

fn transaction<T>(
    task: &impl Tasks,
    desired: bool,
    owns_background: bool,
    save: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let before = task.status()?;
    ensure!(before != Status::Mismatch, "{MISMATCH}");
    ensure!(
        before != Status::Running || owns_background,
        "BACKGROUND_ACTIVE：登入工作已在其他 Windows 工作階段執行。"
    );
    if before.registered() == desired {
        return save();
    }

    fn confirm(task: &impl Tasks, desired: bool) -> Result<()> {
        let after = task.status()?;
        ensure!(
            after != Status::Mismatch && after.registered() == desired,
            "TASK_UNCONFIRMED：無法確認登入工作的變更。"
        );
        Ok(())
    }

    let change = |enabled| {
        if enabled {
            task.create()
        } else {
            task.delete()
        }
    };
    if let Err(error) = change(desired) {
        ensure!(
            task.status().is_ok_and(|after| after == before),
            "SETTINGS_TASK_INCONSISTENT：無法確認登入工作的變更結果。"
        );
        return Err(error);
    }

    match confirm(task, desired).and_then(|()| save()) {
        Ok(value) => Ok(value),
        Err(error) => {
            let rollback = (|| {
                let after = task.status()?;
                ensure!(
                    after != Status::Mismatch && (after != Status::Running || owns_background),
                    "{MISMATCH}"
                );
                if after.registered() != before.registered() {
                    change(before.registered())?;
                    confirm(task, before.registered())?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            if rollback.is_err() {
                bail!("SETTINGS_TASK_INCONSISTENT：無法確認設定及登入工作已復原。");
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn definition(sid: String) -> Definition {
        Definition {
            sid,
            executable: r"C:\隔離測試 A&B\mcp-local-rust.exe".into(),
            directory: r"C:\隔離測試 A&B".into(),
        }
    }

    #[test]
    #[ignore = "read-only check of an explicitly selected installed application's login task"]
    fn installed_login_task_matches_without_starting_it() -> Result<()> {
        let executable = std::env::var("MCP_LOCAL_RUST_STARTUP_CHECK_EXE")?;
        let directory = Path::new(&executable)
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let task = Task::connect(Definition {
            sid: windows_token::current_sid()?,
            executable,
            directory,
        })?;
        ensure!(
            matches!(task.inspect()?, Status::Ready | Status::Running),
            "installed task mismatch"
        );
        Ok(())
    }

    #[test]
    #[ignore = "read-only installed settings and DPAPI preflight; never starts a connection"]
    fn installed_settings_and_connection_dependencies_are_usable() -> Result<()> {
        let executable = std::env::var("MCP_LOCAL_RUST_STARTUP_CHECK_EXE")?;
        let store = crate::desktop_store::Store::new()?;
        let config = store.load()?.context("installed settings missing")?.config;
        config.validate_shared()?;
        crate::connection::validate_vendor_tunnel(&config.tunnel)?;
        ensure!(
            config.auto_start && config.auto_connect,
            "login connection flags disabled"
        );
        let key = config.key()?;
        ensure!(!key.is_empty(), "empty credential");
        crate::security::policy::Store(store.path.clone()).load()?;
        crate::security::audit::Audit(store.path).load()?;
        crate::power_recovery::require_installations_clear()?;
        crate::vendor::Vendor::open(Path::new(&executable))?;
        installed_login_task_matches_without_starting_it()
    }

    #[test]
    fn task_definition_requires_the_fixed_user_action_and_privilege() {
        let definition = definition("S-1-5-21-100".into());
        let xml = definition.xml();
        assert!(definition.matches(&xml, ""));
        let normalized = xml
            .replace("<Enabled>true</Enabled>", "")
            .replace("<RunLevel>LeastPrivilege</RunLevel>", "");
        assert!(definition.matches(&normalized, ""));
        for extra in [
            "<Enabled>false</Enabled>",
            "<Enabled/>",
            "<Delay>PT1H</Delay>",
            "<Enabled>true</Enabled><Enabled>true</Enabled>",
        ] {
            assert!(!definition.matches(
                &normalized.replace("</LogonTrigger>", &format!("{extra}</LogonTrigger>")),
                ""
            ));
        }
        for extra in [
            "<RunLevel>HighestAvailable</RunLevel>",
            "<RunLevel/>",
            "<RunLevel>LeastPrivilege</RunLevel><RunLevel>LeastPrivilege</RunLevel>",
        ] {
            assert!(!definition.matches(
                &normalized.replace("</Principal>", &format!("{extra}</Principal>")),
                ""
            ));
        }
        assert!(definition.matches(
            &xml.replace("S-1-5-21-100", r"FIXTURE\user"),
            r"fixture\User"
        ));
        for bad in [
            xml.replace("LeastPrivilege", "HighestAvailable"),
            xml.replace("InteractiveToken", "Password"),
            xml.replace(ARGUMENT, "--root C:\\"),
            xml.replace("</Command>", "<!-- extra -->.other</Command>"),
            xml.replace("S-1-5-21-100", "S-1-5-21-99"),
            xml.replace("<LogonTrigger>", "<LogonTrigger><Delay>PT1H</Delay>"),
            xml.replace(
                "</Actions>",
                "<Exec><Command>other.exe</Command></Exec></Actions>",
            ),
            xml.replace("IgnoreNew", "Queue"),
            xml.replace("<Count>3", "<Count>99"),
        ] {
            assert!(!definition.matches(&bad, ""), "{bad}");
        }
        for name in ["RunOnlyIfIdle", "RunOnlyIfNetworkAvailable"] {
            let omitted = xml.replace(&format!("<{name}>false</{name}>"), "");
            assert!(definition.matches(&omitted, ""));
            let extra = format!("<{name}>true</{name}></Settings>");
            let changed = omitted.replace("</Settings>", &extra);
            assert!(!definition.matches(&changed, ""), "{name}");
            assert!(!definition.matches(&xml.replace("</Settings>", &extra), ""));
        }
    }

    struct FakeTasks {
        status: Cell<Status>,
        changes: Cell<u8>,
    }

    impl Tasks for FakeTasks {
        fn status(&self) -> Result<Status> {
            Ok(self.status.get())
        }
        fn create(&self) -> Result<()> {
            assert_eq!(self.status.get(), Status::Missing);
            self.status.set(Status::Ready);
            self.changes.set(self.changes.get() + 1);
            Ok(())
        }
        fn delete(&self) -> Result<()> {
            assert!(self.status.get().registered());
            self.status.set(Status::Missing);
            self.changes.set(self.changes.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn task_transaction_restores_previous_registration_and_preserves_unknown_tasks() {
        for before in [Status::Missing, Status::Ready] {
            let fake = FakeTasks {
                status: Cell::new(before),
                changes: Cell::new(0),
            };
            let error = transaction(&fake, !before.registered(), false, || -> Result<()> {
                bail!("fixture write failed")
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "fixture write failed");
            assert_eq!(fake.status.get(), before);
            assert_eq!(fake.changes.get(), 2);
        }
        for before in [Status::Mismatch, Status::Running] {
            let fake = FakeTasks {
                status: Cell::new(before),
                changes: Cell::new(0),
            };
            assert!(
                transaction(&fake, false, false, || -> Result<()> {
                    panic!("must not save")
                })
                .is_err()
            );
            assert_eq!(fake.status.get(), before);
            assert_eq!(fake.changes.get(), 0);
        }
        let fake = FakeTasks {
            status: Cell::new(Status::Missing),
            changes: Cell::new(0),
        };
        assert_eq!(transaction(&fake, true, false, || Ok(42)).unwrap(), 42);
        assert_eq!(fake.status.get(), Status::Ready);
        assert_eq!(fake.changes.get(), 1);
    }

    #[test]
    fn a_second_interactive_launch_signals_only_the_matching_desktop_instance() {
        let fixture = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let state = fixture.path().join("state");
        let first = DesktopInstance::open(&state, true).unwrap().unwrap();
        let independent = DesktopInstance::open(&other.path().join("state"), true)
            .unwrap()
            .unwrap();
        assert!(DesktopInstance::open(&state, false).unwrap().is_none());
        assert!(!first.activated());
        assert!(DesktopInstance::open(&state, true).unwrap().is_none());
        assert!(first.activated());
        assert!(!first.activated());
        assert!(!independent.activated());
        DesktopInstance::request_mode(&state, crate::security::channel::Mode::HostControl).unwrap();
        assert_eq!(
            first.requested_mode(),
            Some(crate::security::channel::Mode::HostControl)
        );
        assert!(first.requested_mode().is_none());
        assert!(independent.requested_mode().is_none());
        drop(first);
        assert!(
            DesktopInstance::request_mode(&state, crate::security::channel::Mode::ReadOnly)
                .is_err()
        );
        assert!(DesktopInstance::open(&state, false).unwrap().is_some());
    }

    #[test]
    fn isolated_settings_cannot_access_the_login_task() {
        let fixture = tempfile::tempdir().unwrap();
        let error = Task::for_store(&fixture.path().join("state"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("LOGIN_STARTUP_UNAVAILABLE"));
    }

    #[test]
    #[ignore = "requires the native local Windows Task Scheduler service"]
    fn task_scheduler_validates_xml_without_registering_a_task() -> Result<()> {
        use windows::{
            Win32::System::TaskScheduler::{IRegisteredTask, TASK_VALIDATE_ONLY},
            core::Interface,
        };

        let task = Task::connect(definition(windows_token::current_sid()?))?;
        let name = BSTR::from(format!(
            "MCP-Local-Rust-ValidateOnly-{}",
            uuid::Uuid::from_bytes(rand_bytes()?)
        ));
        let xml = BSTR::from(task.definition.xml());
        let empty = VARIANT::default();
        let user = VARIANT::from(BSTR::from(&task.definition.sid));
        unsafe {
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            service.Connect(&empty, &empty, &empty, &empty)?;
            let definition = service.NewTask(0)?;
            definition.SetXmlText(&xml)?;
            let mut normalized = BSTR::new();
            definition.XmlText(&mut normalized)?;
            ensure!(
                task.definition
                    .matches(&normalized.to_string(), &task.account),
                "Windows-normalized task XML did not match the application's ownership checks"
            );

            ensure!(
                task.folder
                    .GetTask(&name)
                    .err()
                    .is_some_and(|error| error.code() == ERROR_FILE_NOT_FOUND.to_hresult()),
                "fixture name exists or cannot be queried"
            );
            // Validation may return no task interface even when the HRESULT succeeds.
            let mut registered = std::ptr::null_mut();
            let result = (task.folder.vtable().RegisterTask)(
                task.folder.as_raw(),
                name.as_ptr().cast_mut().cast(),
                xml.as_ptr().cast_mut().cast(),
                TASK_VALIDATE_ONLY.0,
                std::mem::transmute_copy(&user),
                std::mem::transmute_copy(&empty),
                TASK_LOGON_INTERACTIVE_TOKEN,
                std::mem::transmute_copy(&empty),
                &mut registered,
            );
            if !registered.is_null() {
                drop(IRegisteredTask::from_raw(registered));
            }
            result.ok()?;
            ensure!(
                task.folder
                    .GetTask(&name)
                    .err()
                    .is_some_and(|error| error.code() == ERROR_FILE_NOT_FOUND.to_hresult()),
                "validation task must not be registered"
            );
        }
        Ok(())
    }

    fn rand_bytes() -> Result<[u8; 16]> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)?;
        Ok(bytes)
    }
}
