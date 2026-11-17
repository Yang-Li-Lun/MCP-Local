use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
#[cfg(any(feature = "desktop", test))]
use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, IsProcessInJob};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};

pub struct Job(OwnedHandle);

impl AsRawHandle for Job {
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.as_raw_handle()
    }
}

impl Job {
    #[cfg(any(feature = "desktop", test))]
    pub fn connection() -> Result<Self> {
        Self::create(None)
    }

    pub fn host_session() -> Result<Self> {
        Self::create(Some((32, 4 * 1024 * 1024 * 1024)))
    }

    fn create(limits: Option<(u32, usize)>) -> Result<Self> {
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        ensure!(!raw.is_null(), "JOB_CREATE_FAILED：無法建立 Windows Job。");
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut config = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        config.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some((process_limit, memory_limit)) = limits {
            config.BasicLimitInformation.LimitFlags |=
                JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_JOB_MEMORY;
            config.BasicLimitInformation.ActiveProcessLimit = process_limit;
            config.JobMemoryLimit = memory_limit;
        }
        let applied = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&config as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&config) as u32,
            )
        };
        ensure!(
            applied != 0,
            "JOB_LIMIT_FAILED：無法套用 Windows Job 限制。"
        );
        Ok(Self(handle))
    }

    #[cfg(any(feature = "desktop", test))]
    pub fn assign(&self, process: &impl AsRawHandle) -> Result<()> {
        let raw_job = self.0.as_raw_handle();
        let raw_process = process.as_raw_handle();
        let assigned = unsafe { AssignProcessToJobObject(raw_job, raw_process) };
        let mut member = 0;
        ensure!(
            assigned != 0
                && unsafe { IsProcessInJob(raw_process, raw_job, &mut member) } != 0
                && member != 0,
            "JOB_ASSIGN_FAILED：無法確認程序已納入 Windows Job。"
        );
        Ok(())
    }

    pub fn terminate(&self) -> Result<()> {
        const UNCONFIRMED: &str = "JOB_STOP_UNCONFIRMED：無法確認 Windows Job 內所有程序均已停止。";
        ensure!(
            unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } != 0,
            "{UNCONFIRMED}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.active_processes()? == 0 {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "{UNCONFIRMED}");
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn active_processes(&self) -> Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let read = unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of_val(&accounting) as u32,
                null_mut(),
            )
        };
        ensure!(
            read != 0,
            "JOB_ACCOUNTING_FAILED：無法查詢 Windows Job 的程序數量。"
        );
        Ok(accounting.ActiveProcesses)
    }

    #[cfg(test)]
    pub fn limits(&self) -> Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let read = unsafe {
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&limits) as u32,
                null_mut(),
            )
        };
        ensure!(read != 0, "JOB_LIMIT_FAILED");
        Ok(limits)
    }
}
