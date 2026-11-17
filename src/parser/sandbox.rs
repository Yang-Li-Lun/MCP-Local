use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};

use anyhow::{Result, bail, ensure};
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        AccessCheck,
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        DuplicateToken, EqualSid, FreeSid, GENERIC_MAPPING, GetTokenInformation,
        Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile},
        PRIVILEGE_SET, PSID, SECURITY_CAPABILITIES, SecurityIdentification,
        TOKEN_APPCONTAINER_INFORMATION, TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
        TokenAppContainerSid, TokenCapabilities, TokenIsAppContainer,
    },
    System::{
        JobObjects::IsProcessInJob,
        Pipes::CreatePipe,
        SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW},
        Threading::{
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            GetExitCodeProcess, GetProcessMitigationPolicy, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            PROCESS_INFORMATION, ProcessChildProcessPolicy, ResumeThread, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        },
        WindowsProgramming::{
            PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT,
            PROCESS_CREATION_CHILD_PROCESS_RESTRICTED,
        },
    },
};

fn last_error(step: &str) -> anyhow::Error {
    let code = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or_default();
    anyhow::anyhow!("FORMAT_ISOLATION：{step}（Windows {code}）。")
}

fn require(success: i32, step: &str) -> Result<()> {
    if success == 0 {
        return Err(last_error(step));
    }
    Ok(())
}

pub(super) struct Profile {
    name: Vec<u16>,
    sid: PSID,
    registered: bool,
}

impl Profile {
    pub(super) fn create() -> Result<Self> {
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)?;
        let name: Vec<u16> = format!("McpLocalRustParser-{}", crate::listing::hex(&nonce))
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sid = null_mut();
        let status = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        ensure!(
            status == 0,
            "FORMAT_ISOLATION：無法建立解析容器（{status:08X}）。"
        );
        let profile = Self {
            name,
            sid,
            registered: true,
        };
        ensure!(!sid.is_null(), "FORMAT_ISOLATION：解析容器缺少識別碼。");
        Ok(profile)
    }

    pub(super) fn close(&mut self) -> Result<()> {
        if self.registered {
            let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
            ensure!(
                result == 0,
                "FORMAT_ISOLATION：未能清除本次的解析容器（{result:08X}）。"
            );
            self.registered = false;
        }
        Ok(())
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = self.close();
        if !self.sid.is_null() {
            unsafe { FreeSid(self.sid) };
        }
    }
}

fn token_data(token: &OwnedHandle, kind: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    let mut len = 0;
    let raw = token.as_raw_handle();
    let sized = unsafe { GetTokenInformation(raw, kind, null_mut(), 0, &mut len) };
    ensure!(
        sized == 0
            && io::Error::last_os_error().raw_os_error() == Some(ERROR_INSUFFICIENT_BUFFER as i32)
            && (size_of::<u32>() as u32..=16384).contains(&len),
        "FORMAT_ISOLATION：無法確認解析容器權杖資訊。"
    );
    let mut aligned = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
    require(
        unsafe { GetTokenInformation(raw, kind, aligned.as_mut_ptr().cast(), len, &mut len) },
        "無法讀取解析容器權杖",
    )?;
    Ok(aligned)
}

fn verify(process: HANDLE, expected_sid: Option<PSID>) -> Result<()> {
    let mut raw = null_mut();
    let mut access = TOKEN_QUERY;
    if expected_sid.is_some() {
        access |= TOKEN_DUPLICATE;
    }
    require(
        unsafe { OpenProcessToken(process, access, &mut raw) },
        "無法開啟解析程序權杖",
    )?;
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let app = token_data(&token, TokenIsAppContainer)?;
    let caps = token_data(&token, TokenCapabilities)?;
    let first_dword = |buffer: &[usize]| unsafe { buffer.as_ptr().cast::<u32>().read() };
    ensure!(
        first_dword(&app) == 1 && first_dword(&caps) == 0,
        "FORMAT_ISOLATION：解析程序權杖未符合零能力的 AppContainer。"
    );
    if let Some(sid) = expected_sid {
        let actual = token_data(&token, TokenAppContainerSid)?;
        let actual = unsafe { &*actual.as_ptr().cast::<TOKEN_APPCONTAINER_INFORMATION>() };
        ensure!(
            !actual.TokenAppContainer.is_null()
                && unsafe { EqualSid(sid, actual.TokenAppContainer) } != 0,
            "FORMAT_ISOLATION：解析程序權杖的容器識別碼不同。"
        );
        verify_package_access(&token)?;
    }
    let mut policy = 0u32;
    require(
        unsafe {
            GetProcessMitigationPolicy(
                process,
                ProcessChildProcessPolicy,
                (&mut policy as *mut u32).cast(),
                size_of::<u32>(),
            )
        },
        "無法查詢解析程序的子程序限制",
    )?;
    ensure!(
        policy & 1 == 1,
        "FORMAT_ISOLATION：解析程序未禁止建立子程序。"
    );
    Ok(())
}

fn verify_package_access(token: &OwnedHandle) -> Result<()> {
    let mut raw = null_mut();
    require(
        unsafe { DuplicateToken(token.as_raw_handle(), SecurityIdentification, &mut raw) },
        "無法取得隔離權杖供權限檢查",
    )?;
    let inspection = unsafe { OwnedHandle::from_raw_handle(raw) };
    // An LPAC must pass the restricted-package grant and fail the ordinary
    // All Application Packages grant. The DACLs exist only in memory.
    for (sddl, expected) in [
        (
            windows_sys::w!("O:SYG:SYD:(A;;0x1;;;WD)(A;;0x1;;;S-1-15-2-2)"),
            true,
        ),
        (
            windows_sys::w!("O:SYG:SYD:(A;;0x1;;;WD)(A;;0x1;;;AC)"),
            false,
        ),
    ] {
        let mut descriptor = null_mut();
        require(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl,
                    SDDL_REVISION_1,
                    &mut descriptor,
                    null_mut(),
                )
            },
            "無法配置解析權限檢查",
        )?;
        let mut privileges = PRIVILEGE_SET::default();
        let mut len = size_of::<PRIVILEGE_SET>() as u32;
        let (mut granted, mut access) = (0, 0);
        let mapping = GENERIC_MAPPING {
            GenericRead: 1,
            GenericWrite: 2,
            GenericExecute: 4,
            GenericAll: 7,
        };
        let checked = require(
            unsafe {
                AccessCheck(
                    descriptor,
                    inspection.as_raw_handle(),
                    1,
                    &mapping,
                    &mut privileges,
                    &mut len,
                    &mut granted,
                    &mut access,
                )
            },
            "無法確認 AppContainer 的有效權限",
        );
        unsafe { LocalFree(descriptor) };
        checked?;
        ensure!(
            (access != 0 && granted & 1 == 1) == expected,
            "FORMAT_ISOLATION：解析程序的有效權限不符合低權限 AppContainer。"
        );
    }
    Ok(())
}

pub(super) fn verify_worker() -> Result<()> {
    verify(unsafe { GetCurrentProcess() }, None)
}

struct Attributes(Vec<usize>);

impl Attributes {
    fn new(count: u32) -> Result<Self> {
        let mut len = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut len) };
        ensure!(
            (1..=16384).contains(&len),
            "FORMAT_ISOLATION：無法配置解析程序屬性。"
        );
        let mut buffer = vec![0usize; len.div_ceil(size_of::<usize>())];
        require(
            unsafe {
                InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut len)
            },
            "無法初始化 Windows 程序屬性",
        )?;
        Ok(Self(buffer))
    }

    fn raw(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    // The caller keeps every referenced value alive until these attributes are dropped.
    unsafe fn add<T>(&mut self, id: u32, value: &T) -> Result<()> {
        require(
            unsafe {
                UpdateProcThreadAttribute(
                    self.raw(),
                    0,
                    id as usize,
                    (value as *const T).cast(),
                    size_of::<T>(),
                    null_mut(),
                    null(),
                )
            },
            "無法設定解析程序隔離屬性",
        )
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.raw()) };
    }
}

fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    require(
        unsafe { CreatePipe(&mut read, &mut write, null(), 0) },
        "無法建立解析程序管線",
    )?;
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

fn inheritable(handle: &impl AsRawHandle) -> Result<OwnedHandle> {
    let this = unsafe { GetCurrentProcess() };
    let mut out = null_mut();
    require(
        unsafe {
            DuplicateHandle(
                this,
                handle.as_raw_handle(),
                this,
                &mut out,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        },
        "無法傳遞解析程序管線",
    )?;
    Ok(unsafe { OwnedHandle::from_raw_handle(out) })
}

fn system_path(api: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Result<Vec<u16>> {
    let mut path = vec![0; 32768];
    let len = unsafe { api(path.as_mut_ptr(), path.len() as u32) } as usize;
    ensure!(
        (1..path.len()).contains(&len),
        "FORMAT_ISOLATION：無法取得 Windows 系統路徑。"
    );
    path.truncate(len);
    Ok(path)
}

pub(super) struct Process(OwnedHandle);

impl Process {
    pub(super) fn poll(&self) -> Result<Option<bool>> {
        let handle = self.0.as_raw_handle();
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut status = 0;
                require(
                    unsafe { GetExitCodeProcess(handle, &mut status) },
                    "無法取得解析程序結束狀態",
                )?;
                Ok(Some(status == 0))
            }
            _ => Err(last_error("無法等待解析程序")),
        }
    }

    pub(super) fn stop(&self) -> Result<()> {
        let handle = self.0.as_raw_handle();
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
            unsafe { TerminateProcess(handle, 1) };
        }
        ensure!(
            unsafe { WaitForSingleObject(handle, 5000) } == WAIT_OBJECT_0,
            "FORMAT_ISOLATION：解析程序未能停止。"
        );
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(super) fn launch(
    profile: &Profile,
    exe: &Path,
    arguments: &str,
    job: &OwnedHandle,
) -> Result<(Process, File, File)> {
    let name: Vec<_> = exe.as_os_str().encode_wide().chain([0]).collect();
    let mut command: Vec<_> = "\""
        .encode_utf16()
        .chain(exe.as_os_str().encode_wide())
        .chain("\" ".encode_utf16())
        .chain(arguments.encode_utf16())
        .chain([0])
        .collect();
    let windows = system_path(GetSystemWindowsDirectoryW)?;
    let mut system = system_path(GetSystemDirectoryW)?;
    // AppContainer startup requires user environment fields. Use a trusted
    // Windows directory for those fields and omit the caller's environment.
    let mut env = Vec::new();
    for (key, value) in [
        ("APPDATA=", &system),
        ("LOCALAPPDATA=", &system),
        ("SystemRoot=", &windows),
        ("TEMP=", &system),
        ("TMP=", &system),
        ("USERPROFILE=", &system),
        ("WINDIR=", &windows),
    ] {
        env.extend(key.encode_utf16().chain(value.iter().copied()).chain([0]));
    }
    env.push(0);
    system.push(0);

    let (read_in, input) = pipe()?;
    let (output, write_out) = pipe()?;
    let null_output = OpenOptions::new().write(true).open(r"\\.\NUL")?;
    let read_in = inheritable(&read_in)?;
    let write_out = inheritable(&write_out)?;
    let stderr = inheritable(&null_output)?;
    let handles = [
        read_in.as_raw_handle(),
        write_out.as_raw_handle(),
        stderr.as_raw_handle(),
    ];
    let jobs = [job.as_raw_handle()];
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: profile.sid,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let process_policy = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
    let package_policy = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT;
    let mut attrs = Attributes::new(5)?;
    unsafe {
        attrs.add(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        attrs.add(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
        attrs.add(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities)?;
        attrs.add(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &process_policy)?;
        attrs.add(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            &package_policy,
        )?;
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attrs.raw();
    let mut info = PROCESS_INFORMATION::default();
    let flags = DETACHED_PROCESS
        | CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT;
    require(
        unsafe {
            CreateProcessW(
                name.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                env.as_ptr().cast(),
                system.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        },
        "無法啟動隔離的解析程序",
    )?;
    let process = Process(unsafe { OwnedHandle::from_raw_handle(info.hProcess) });
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let mut assigned = 0;
    require(
        unsafe { IsProcessInJob(info.hProcess, job.as_raw_handle(), &mut assigned) },
        "無法查詢解析程序的 Job",
    )?;
    if assigned == 0 {
        bail!("FORMAT_ISOLATION：解析程序未加入資源限制 Job。");
    }
    verify(info.hProcess, Some(profile.sid))?;
    let resumed = unsafe { ResumeThread(thread.as_raw_handle()) };
    ensure!(resumed == 1, "FORMAT_ISOLATION：無法啟動已驗證的解析程序。");
    Ok((process, File::from(input), File::from(output)))
}
#[cfg(test)]
mod tests;
