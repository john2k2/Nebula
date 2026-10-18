//! A second connected process must not replace the first window's service session.
use std::{ffi::c_void, io};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

pub struct Instance(*mut c_void);

impl Instance {
    pub fn acquire() -> io::Result<Option<Self>> {
        Self::named("Local\\XuperWindows-Connected-v1")
    }

    fn named(name: &str) -> io::Result<Option<Self>> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // The named object exists while any process holds its handle; Windows
        // releases it on a crash too. No persistent lock file can become stale.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        if handle.is_null() { return Err(io::Error::from_raw_os_error(error as i32)); }
        if error == 183 {
            unsafe { CloseHandle(handle); }
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

impl Drop for Instance {
    fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
}

pub fn show_existing() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut c_void;
        fn ShowWindow(window: *mut c_void, command: i32) -> i32;
        fn SetForegroundWindow(window: *mut c_void) -> i32;
    }
    // Keep the existing mutex so an older preview and Nébula cannot activate
    // the same saved installation concurrently during the transition.
    for title in ["Nébula · Escritorio", "Xuper · Escritorio"] {
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let window = FindWindowW(std::ptr::null(), title.as_ptr());
            if !window.is_null() { ShowWindow(window, 9); SetForegroundWindow(window); return; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_connected_instance_until_the_last_handle_closes() {
        let name = format!("Local\\XuperWindows-QA-{}", uuid::Uuid::new_v4());
        let first = Instance::named(&name).unwrap().unwrap();
        assert!(Instance::named(&name).unwrap().is_none());
        drop(first);
        assert!(Instance::named(&name).unwrap().is_some());
    }
}
