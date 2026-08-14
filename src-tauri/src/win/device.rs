//! Device-interface enumeration and handle ownership.

use super::{to_wide, wide_to_string, WinError, WinResult};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_ITEMS, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// `GUID_DEVICE_BATTERY` from batclass.h — the battery device interface class.
pub const GUID_DEVICE_BATTERY: GUID = GUID::from_u128(0x72631e54_78a4_11d0_bcf7_00aa00b7b32a);

/// `GUID_DEVINTERFACE_DISK` from ntddstor.h.
pub const GUID_DEVINTERFACE_DISK: GUID = GUID::from_u128(0x53f56307_b6bf_11d0_94f2_00a0c91efb8b);

/// A `HANDLE` that closes itself.
pub struct SafeHandle(pub HANDLE);

impl SafeHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for SafeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Enumerate the device-interface paths registered for `class_guid`.
///
/// Returns an empty vector when the machine simply has none of that device — a desktop
/// with no battery is a normal outcome here, not an error.
pub fn interface_paths(class_guid: &GUID) -> WinResult<Vec<String>> {
    let mut paths = Vec::new();

    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            Some(class_guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(|_| WinError::last("SetupDiGetClassDevsW"))?;

        let mut index = 0u32;
        loop {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };

            if SetupDiEnumDeviceInterfaces(dev_info, None, class_guid, index, &mut iface).is_err() {
                // ERROR_NO_MORE_ITEMS is the normal loop terminator.
                let code = windows::Win32::Foundation::GetLastError().0;
                if code != ERROR_NO_MORE_ITEMS.0 {
                    let _ = SetupDiDestroyDeviceInfoList(dev_info);
                    return Err(WinError::new("SetupDiEnumDeviceInterfaces", code));
                }
                break;
            }

            // First call sizes the buffer; it is expected to "fail" with
            // ERROR_INSUFFICIENT_BUFFER, so the error is intentionally ignored.
            let mut required: u32 = 0;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface,
                None,
                0,
                Some(&mut required),
                None,
            );

            if required > 0 {
                let mut buffer = vec![0u8; required as usize];
                let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                // cbSize describes the *fixed header*, not the whole allocation:
                // 6 on 32-bit, 8 on 64-bit. Getting this wrong fails with
                // ERROR_INVALID_USER_BUFFER.
                (*detail).cbSize =
                    std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

                if SetupDiGetDeviceInterfaceDetailW(
                    dev_info,
                    &iface,
                    Some(detail),
                    required,
                    None,
                    None,
                )
                .is_ok()
                {
                    // DevicePath is a flexible array member declared as [u16; 1].
                    let path_ptr = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
                    let max_chars =
                        (required as usize - std::mem::size_of::<u32>()) / std::mem::size_of::<u16>();
                    let slice = std::slice::from_raw_parts(path_ptr, max_chars);
                    let path = wide_to_string(slice);
                    if !path.is_empty() {
                        paths.push(path);
                    }
                }
            }

            index += 1;
        }

        let _ = SetupDiDestroyDeviceInfoList(dev_info);
    }

    Ok(paths)
}

/// Open a device interface path for read/write IOCTLs.
pub fn open_device(path: &str) -> WinResult<SafeHandle> {
    let wide = to_wide(path);
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|_| WinError::last("CreateFileW"))?;

        if handle == INVALID_HANDLE_VALUE {
            return Err(WinError::last("CreateFileW"));
        }
        Ok(SafeHandle(handle))
    }
}

/// Open a physical drive by index (`\\.\PhysicalDrive0`) with read-only intent.
///
/// Read-only matters: this tool inspects other people's machines and must never be
/// the reason a stranger's disk changes state.
pub fn open_physical_drive(index: u32) -> WinResult<SafeHandle> {
    let path = format!(r"\\.\PhysicalDrive{index}");
    let wide = to_wide(&path);
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            0, // no access rights: enough for IOCTL_STORAGE_QUERY_PROPERTY
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .map_err(|_| WinError::last("CreateFileW(PhysicalDrive)"))?;

        if handle == INVALID_HANDLE_VALUE {
            return Err(WinError::last("CreateFileW(PhysicalDrive)"));
        }
        Ok(SafeHandle(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_interface_guid_matches_batclass_h() {
        // {72631E54-78A4-11D0-BCF7-00AA00B7B32A}
        assert_eq!(GUID_DEVICE_BATTERY.data1, 0x72631e54);
        assert_eq!(GUID_DEVICE_BATTERY.data2, 0x78a4);
        assert_eq!(GUID_DEVICE_BATTERY.data3, 0x11d0);
    }

    #[test]
    fn enumerating_batteries_never_errors_on_this_machine() {
        // A desktop legitimately returns an empty list; either way this must not error.
        let r = interface_paths(&GUID_DEVICE_BATTERY);
        assert!(r.is_ok(), "enumeration failed: {:?}", r.err());
    }
}
