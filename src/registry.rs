use std::ffi::c_void;
use std::ptr::null_mut;

use crate::wide;

type HKey = *mut c_void;

const HKEY_CURRENT_USER: HKey = 0x8000_0001usize as HKey;
const ERROR_SUCCESS: i32 = 0;

pub const KEY_QUERY_VALUE: u32 = 0x0001;
pub const KEY_SET_VALUE: u32 = 0x0002;
pub const REG_SZ: u32 = 1;
pub const REG_DWORD: u32 = 4;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: HKey,
        subkey: *const u16,
        options: u32,
        desired: u32,
        result: *mut HKey,
    ) -> i32;
    fn RegCreateKeyExW(
        key: HKey,
        subkey: *const u16,
        reserved: u32,
        class: *mut u16,
        options: u32,
        desired: u32,
        security_attributes: *mut c_void,
        result: *mut HKey,
        disposition: *mut u32,
    ) -> i32;
    fn RegQueryValueExW(
        key: HKey,
        value_name: *const u16,
        reserved: *mut u32,
        value_type: *mut u32,
        data: *mut u8,
        data_size: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: HKey,
        value_name: *const u16,
        reserved: u32,
        value_type: u32,
        data: *const u8,
        data_size: u32,
    ) -> i32;
    fn RegDeleteValueW(key: HKey, value_name: *const u16) -> i32;
    fn RegCloseKey(key: HKey) -> i32;
}

pub struct Key(HKey);

impl Key {
    pub fn open(subkey: &str, access: u32) -> Option<Self> {
        let subkey = wide(subkey);
        let mut key = null_mut();
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, access, &mut key) };
        (status == ERROR_SUCCESS).then_some(Self(key))
    }

    pub fn create(subkey: &str, access: u32) -> Option<Self> {
        let subkey = wide(subkey);
        let mut key = null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null_mut(),
                0,
                access,
                null_mut(),
                &mut key,
                null_mut(),
            )
        };
        (status == ERROR_SUCCESS).then_some(Self(key))
    }

    pub fn query(&self, name: &str, data: &mut [u8]) -> Option<(u32, usize)> {
        let name = wide(name);
        let mut value_type = 0u32;
        let mut size = data.len() as u32;
        let buffer = if data.is_empty() {
            null_mut()
        } else {
            data.as_mut_ptr()
        };
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                name.as_ptr(),
                null_mut(),
                &mut value_type,
                buffer,
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then_some((value_type, size as usize))
    }

    pub fn set(&self, name: &str, value_type: u32, data: &[u8]) -> bool {
        let name = wide(name);
        let status = unsafe {
            RegSetValueExW(
                self.0,
                name.as_ptr(),
                0,
                value_type,
                data.as_ptr(),
                data.len() as u32,
            )
        };
        status == ERROR_SUCCESS
    }

    pub fn delete(&self, name: &str) {
        let name = wide(name);
        unsafe { RegDeleteValueW(self.0, name.as_ptr()) };
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
