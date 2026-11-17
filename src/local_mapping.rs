use std::{
    marker::PhantomData,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::{self, null},
    rc::Rc,
};

use anyhow::{Result, ensure};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
    },
    System::{
        Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
        },
        Threading::{
            CreateMutexW, MUTEX_ALL_ACCESS, OpenMutexW, ReleaseMutex, WaitForSingleObject,
        },
    },
};

pub struct Mapping {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
    mutex: OwnedHandle,
    _mapping: OwnedHandle,
}

// The view is accessed only while the calling thread owns the named mutex.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn connect(name: &str, size: usize, create: bool) -> Result<Self> {
        ensure!(
            (1..=u32::MAX as usize).contains(&size),
            "本機共享記憶體容量無效。"
        );
        let wide = |name: &str| name.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let section_name = wide(name);
        let mutex_name = wide(&format!("{name}.Mutex"));
        let mapping = unsafe {
            if create {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    null(),
                    PAGE_READWRITE,
                    0,
                    size as u32,
                    section_name.as_ptr(),
                )
            } else {
                OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, section_name.as_ptr())
            }
        };
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        ensure!(!mapping.is_null(), "無法開啟本機共享記憶體。");
        let mapping = unsafe { OwnedHandle::from_raw_handle(mapping) };
        ensure!(!create || !exists, "本機共享記憶體名稱已使用。");
        let mutex = unsafe {
            if create {
                CreateMutexW(null(), 0, mutex_name.as_ptr())
            } else {
                OpenMutexW(MUTEX_ALL_ACCESS, 0, mutex_name.as_ptr())
            }
        };
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        ensure!(!mutex.is_null(), "無法開啟本機共享記憶體鎖。");
        let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
        ensure!(!create || !exists, "本機共享記憶體鎖名稱已使用。");
        let view =
            unsafe { MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_ALL_ACCESS, 0, 0, size) };
        ensure!(!view.Value.is_null(), "無法映射本機共享記憶體。");
        Ok(Self {
            view,
            size,
            mutex,
            _mapping: mapping,
        })
    }

    pub fn lock(&self, timeout_ms: u32, accept_abandoned: bool) -> Option<Locked<'_>> {
        let state = unsafe { WaitForSingleObject(self.mutex.as_raw_handle(), timeout_ms) };
        if state == WAIT_OBJECT_0 || state == WAIT_ABANDONED && accept_abandoned {
            Some(Locked {
                mapping: self,
                same_thread: PhantomData,
            })
        } else {
            if state == WAIT_ABANDONED {
                unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
            }
            None
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { UnmapViewOfFile(self.view) };
    }
}

pub struct Locked<'a> {
    mapping: &'a Mapping,
    same_thread: PhantomData<Rc<()>>,
}

impl Locked<'_> {
    fn at(&self, offset: usize, length: usize) -> *mut u8 {
        assert!(offset <= self.mapping.size && length <= self.mapping.size - offset);
        unsafe { self.mapping.view.Value.cast::<u8>().add(offset) }
    }

    pub fn read(&self, offset: usize, into: &mut [u8]) {
        unsafe {
            ptr::copy_nonoverlapping(self.at(offset, into.len()), into.as_mut_ptr(), into.len())
        };
    }

    pub fn write(&self, offset: usize, bytes: &[u8]) {
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.at(offset, bytes.len()), bytes.len())
        };
    }

    pub fn zero(&self, offset: usize, length: usize) {
        unsafe { ptr::write_bytes(self.at(offset, length), 0, length) };
    }

    #[cfg(feature = "desktop")]
    pub fn number(&self, offset: usize) -> u64 {
        let mut bytes = [0; 8];
        self.read(offset, &mut bytes);
        u64::from_le_bytes(bytes)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.mapping.mutex.as_raw_handle()) };
    }
}
