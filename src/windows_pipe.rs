use std::{io, os::windows::io::AsRawHandle, ptr::null_mut};

use windows_sys::Win32::{Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe};

pub fn pending_bytes(pipe: &impl AsRawHandle) -> io::Result<Option<usize>> {
    let mut pending = 0;
    let read = unsafe {
        PeekNamedPipe(
            pipe.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            &mut pending,
            null_mut(),
        )
    };
    if read != 0 {
        return Ok(Some(pending as usize));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
        Ok(None)
    } else {
        Err(error)
    }
}
