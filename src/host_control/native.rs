use crate::{filesystem::known_folder, windows_job::Job, windows_pipe::pending_bytes};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    fs::File,
    io::{Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::IsProcessInJob,
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            GetExitCodeProcess, InitializeProcThreadAttributeList,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
            ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
    UI::Shell::FOLDERID_Windows,
};

fn check(value: bool) -> Result<()> {
    ensure!(
        value,
        "HOST_PROCESS_FAILED：Windows 程序隔離操作失敗（{}）。",
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    );
    Ok(())
}

struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast()) };
    }
}

fn pipe() -> Result<(File, File)> {
    let (mut read, mut write) = (null_mut(), null_mut());
    check(unsafe { CreatePipe(&mut read, &mut write, null(), 8192) } != 0)?;
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}

fn inherit(file: &File) -> Result<OwnedHandle> {
    let mut result = null_mut();
    let current = unsafe { GetCurrentProcess() };
    check(
        unsafe {
            DuplicateHandle(
                current,
                file.as_raw_handle(),
                current,
                &mut result,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } != 0,
    )?;
    Ok(unsafe { OwnedHandle::from_raw_handle(result) })
}

pub struct Process {
    handle: OwnedHandle,
    thread: OwnedHandle,
    pub job: Job,
    pub input: Option<File>,
    output: File,
    pub pid: u32,
}

impl Process {
    pub fn prepare(command: &str, cwd: &Path) -> Result<Self> {
        let windows = known_folder(&FOLDERID_Windows)?;
        let exe = windows.join("System32/WindowsPowerShell/v1.0/powershell.exe");
        // No profile, script file, or command-line interpolation.
        let script = format!(
            "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; [Console]::InputEncoding = [Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; try {{ & {{\n{command}\n}} }} catch {{ [Console]::Error.WriteLine($_.ToString()); exit 1 }}"
        );
        let encoded = STANDARD.encode(
            script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        Self::launch(
            &exe,
            &format!(
                "-NoLogo -NoProfile -NonInteractive -InputFormat Text -OutputFormat Text -EncodedCommand {encoded}"
            ),
            cwd,
        )
    }

    pub(super) fn launch(exe: &Path, arguments: &str, cwd: &Path) -> Result<Self> {
        crate::windows_token::require_standard_user("HOST_TOKEN_REJECTED")?;
        let name: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let mut command: Vec<u16> = "\""
            .encode_utf16()
            .chain(exe.as_os_str().encode_wide())
            .chain("\" ".encode_utf16())
            .chain(arguments.encode_utf16())
            .chain([0])
            .collect();
        ensure!(command.len() < 32767, "HOST_COMMAND_LIMIT");
        let directory: Vec<u16> = cwd.as_os_str().encode_wide().chain([0]).collect();
        let windows = known_folder(&FOLDERID_Windows)?;
        let mut environment = Vec::new();
        for (key, value) in super::environment::build(std::env::vars_os(), &windows, cwd) {
            environment.extend(
                key.encode_utf16()
                    .chain(['=' as u16])
                    .chain(value.encode_wide())
                    .chain([0]),
            );
        }
        environment.push(0);
        let (read, input) = pipe()?;
        let (output, write) = pipe()?;
        let read = inherit(&read)?;
        let write = inherit(&write)?;
        let handles = [read.as_raw_handle(), write.as_raw_handle()];
        let job = Job::host_session()?;
        let jobs = [job.as_raw_handle()];
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size) };
        ensure!((1..=16384).contains(&size), "HOST_PROCESS_ATTRIBUTES");
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        check(
            unsafe {
                InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), 2, 0, &mut size)
            } != 0,
        )?;
        let mut attributes = Attributes(buffer);
        for (key, values) in [
            (PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_slice()),
            (PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs.as_slice()),
        ] {
            check(
                unsafe {
                    UpdateProcThreadAttribute(
                        attributes.0.as_mut_ptr().cast(),
                        0,
                        key as usize,
                        values.as_ptr().cast(),
                        size_of_val(values),
                        null_mut(),
                        null(),
                    )
                } != 0,
            )?;
        }
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = handles[0];
        startup.StartupInfo.hStdOutput = handles[1];
        startup.StartupInfo.hStdError = handles[1];
        startup.lpAttributeList = attributes.0.as_mut_ptr().cast();
        let mut info = PROCESS_INFORMATION::default();
        check(
            unsafe {
                CreateProcessW(
                    name.as_ptr(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_NO_WINDOW
                        | CREATE_SUSPENDED
                        | CREATE_UNICODE_ENVIRONMENT
                        | EXTENDED_STARTUPINFO_PRESENT,
                    environment.as_ptr().cast(),
                    directory.as_ptr(),
                    &startup.StartupInfo,
                    &mut info,
                )
            } != 0,
        )?;
        let process = Self {
            handle: unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
            thread: unsafe { OwnedHandle::from_raw_handle(info.hThread) },
            job,
            input: Some(input),
            output,
            pid: info.dwProcessId,
        };
        let mut member = 0;
        check(
            unsafe {
                IsProcessInJob(
                    process.handle.as_raw_handle(),
                    process.job.as_raw_handle(),
                    &mut member,
                )
            } != 0
                && member != 0,
        )?;
        Ok(process)
    }

    pub fn resume(&self) -> Result<()> {
        check(unsafe { ResumeThread(self.thread.as_raw_handle()) } == 1)
    }

    pub fn poll(&self) -> Result<Option<i32>> {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                check(unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) } != 0)?;
                Ok(Some(code as i32))
            }
            _ => {
                check(false)?;
                unreachable!()
            }
        }
    }

    pub fn drain(&mut self, bytes: &mut Vec<u8>, maximum: usize) -> Result<()> {
        // One bounded poll per iteration so a noisy child cannot starve revocation.
        if let Some(pending) = pending_bytes(&self.output)? {
            ensure!(
                bytes.len().saturating_add(pending) <= maximum,
                "HOST_OUTPUT_LIMIT"
            );
            if pending != 0 {
                let mut chunk = vec![0; pending.min(8192)];
                let count = self.output.read(&mut chunk)?;
                bytes.extend_from_slice(&chunk[..count]);
            }
        }
        Ok(())
    }

    pub fn write(mut input: File, text: &[u8]) -> Result<()> {
        input.write_all(text)?;
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.job.terminate();
    }
}
