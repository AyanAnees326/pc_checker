//! IOCTL control-code construction and the device-control call itself.

use super::{WinError, WinResult};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;

// CTL_CODE layout, from winioctl.h:
//   (DeviceType << 16) | (Access << 14) | (Function << 2) | Method
pub const METHOD_BUFFERED: u32 = 0;
pub const FILE_ANY_ACCESS: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 1;
pub const FILE_WRITE_ACCESS: u32 = 2;

pub const FILE_DEVICE_BATTERY: u32 = 0x0000_0029;
pub const FILE_DEVICE_MASS_STORAGE: u32 = 0x0000_002d;
pub const FILE_DEVICE_CONTROLLER: u32 = 0x0000_0004;

pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

// --- Battery class driver (batclass.h) ---
pub const IOCTL_BATTERY_QUERY_TAG: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x10, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_BATTERY_QUERY_INFORMATION: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x11, METHOD_BUFFERED, FILE_READ_ACCESS);
pub const IOCTL_BATTERY_QUERY_STATUS: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x13, METHOD_BUFFERED, FILE_READ_ACCESS);

// --- Storage (winioctl.h / ntddstor.h) ---
pub const IOCTL_STORAGE_QUERY_PROPERTY: u32 =
    ctl_code(FILE_DEVICE_MASS_STORAGE, 0x0500, METHOD_BUFFERED, FILE_ANY_ACCESS);
pub const IOCTL_SCSI_PASS_THROUGH: u32 =
    ctl_code(FILE_DEVICE_CONTROLLER, 0x0401, METHOD_BUFFERED, FILE_READ_ACCESS);

pub const FILE_DEVICE_DISK: u32 = 0x0000_0007;

/// `SMART_GET_VERSION` — probes whether the driver supports the legacy SMART IOCTLs.
pub const SMART_GET_VERSION: u32 =
    ctl_code(FILE_DEVICE_DISK, 0x0020, METHOD_BUFFERED, FILE_READ_ACCESS);

/// `SMART_RCV_DRIVE_DATA` — the legacy SMART read path.
///
/// Older and narrower than ATA pass-through, but supported by AHCI drivers that
/// reject `IOCTL_ATA_PASS_THROUGH` outright, so it is tried first.
pub const SMART_RCV_DRIVE_DATA: u32 = ctl_code(
    FILE_DEVICE_DISK,
    0x0022,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

/// `IOCTL_ATA_PASS_THROUGH` — the modern SATA SMART path.
///
/// Declared with write access by Windows even though SMART READ DATA only transfers
/// data inward; the access bits describe the IOCTL, not our intent.
pub const IOCTL_ATA_PASS_THROUGH: u32 = ctl_code(
    FILE_DEVICE_CONTROLLER,
    0x040b,
    METHOD_BUFFERED,
    FILE_READ_ACCESS | FILE_WRITE_ACCESS,
);

/// Issue a `DeviceIoControl` with a typed input and a byte output buffer.
///
/// Returns the number of bytes the driver actually wrote, which callers must check —
/// several storage drivers succeed while returning a short buffer, and treating that
/// as a full struct is how you end up reporting garbage SMART values.
pub fn device_io_control(
    handle: HANDLE,
    code: u32,
    input: Option<&[u8]>,
    output: &mut [u8],
    context: &'static str,
) -> WinResult<u32> {
    let mut returned: u32 = 0;
    let (in_ptr, in_len) = match input {
        Some(b) => (b.as_ptr() as *const core::ffi::c_void, b.len() as u32),
        None => (std::ptr::null(), 0),
    };

    unsafe {
        DeviceIoControl(
            handle,
            code,
            Some(in_ptr),
            in_len,
            Some(output.as_mut_ptr() as *mut core::ffi::c_void),
            output.len() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| WinError::from_win(context, &e))?;
    }

    Ok(returned)
}

/// Reinterpret the head of a byte buffer as `T`.
///
/// # Safety
/// `T` must be `#[repr(C)]` and plain-old-data, and `bytes` must hold at least
/// `size_of::<T>()` bytes actually written by the driver.
pub unsafe fn read_struct<T: Copy>(bytes: &[u8]) -> Option<T> {
    if bytes.len() < std::mem::size_of::<T>() {
        return None;
    }
    Some(std::ptr::read_unaligned(bytes.as_ptr() as *const T))
}

/// View any `#[repr(C)]` value as bytes, for use as IOCTL input.
pub fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These constants are load-bearing: a wrong CTL_CODE silently returns
    // ERROR_INVALID_FUNCTION and every battery field would read as unavailable.
    // Values cross-checked against batclass.h / ntddstor.h.
    #[test]
    fn battery_ioctl_codes_match_batclass_h() {
        assert_eq!(IOCTL_BATTERY_QUERY_TAG, 0x0029_4040);
        assert_eq!(IOCTL_BATTERY_QUERY_INFORMATION, 0x0029_4044);
        assert_eq!(IOCTL_BATTERY_QUERY_STATUS, 0x0029_404C);
    }

    #[test]
    fn storage_ioctl_codes_match_winioctl_h() {
        assert_eq!(IOCTL_STORAGE_QUERY_PROPERTY, 0x002D_1400);
    }

    #[test]
    fn ctl_code_packs_fields_in_the_documented_order() {
        // DeviceType 0x29, Function 0x10, Method 0, Access 1
        assert_eq!(ctl_code(0x29, 0x10, 0, 1), (0x29 << 16) | (1 << 14) | (0x10 << 2));
    }

    #[test]
    fn read_struct_rejects_short_buffers() {
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct Big {
            a: u64,
            b: u64,
        }
        let short = [0u8; 4];
        assert!(unsafe { read_struct::<Big>(&short) }.is_none());
    }
}
