use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
};

use anyhow::{Result, ensure};
use windows_sys::Win32::{
    Security::{GetTokenInformation, TOKEN_QUERY, TokenElevation, TokenIsAppContainer},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub fn require_standard_user(rejected: &str) -> Result<()> {
    let token = current_process_token(rejected)?;
    for class in [TokenElevation, TokenIsAppContainer] {
        let mut flag = 0u32;
        let mut size = 0u32;
        let width = size_of::<u32>() as u32;
        let status = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                (&mut flag as *mut u32).cast(),
                width,
                &mut size,
            )
        };
        ensure!(status != 0 && size == width && flag == 0, "{rejected}");
    }
    Ok(())
}

fn current_process_token(rejected: &str) -> Result<OwnedHandle> {
    let mut raw = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } != 0,
        "{rejected}"
    );
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

#[cfg(feature = "desktop")]
pub fn current_sid() -> Result<String> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{Authorization::ConvertSidToStringSidW, TOKEN_USER, TokenUser},
    };

    const ERROR: &str = "無法確認目前 Windows 使用者。";
    let token = current_process_token(ERROR)?;
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut size,
        )
    };
    ensure!(
        (size_of::<TOKEN_USER>()..=4096).contains(&(size as usize)),
        "{ERROR}"
    );
    let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                data.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } != 0,
        "{ERROR}"
    );
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut raw = ptr::null_mut();
    ensure!(
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) } != 0 && !raw.is_null(),
        "{ERROR}"
    );
    let source = unsafe { windows::core::PCWSTR(raw).to_string() };
    unsafe { LocalFree(raw.cast()) };
    let sid = source.map_err(|_| anyhow::anyhow!(ERROR))?;
    ensure!(
        sid.starts_with("S-1-")
            && sid
                .bytes()
                .skip(2)
                .all(|byte| byte.is_ascii_digit() || byte == b'-'),
        "{ERROR}"
    );
    Ok(sid)
}
