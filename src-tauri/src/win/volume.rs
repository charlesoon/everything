use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_QUERY_USN_JOURNAL;
use windows::core::PCWSTR;

#[derive(Debug)]
pub struct VolumeHandle {
    handle: HANDLE,
}

// SAFETY: NTFS volume handles are safe to use across threads.
// The underlying kernel object is thread-safe.
unsafe impl Send for VolumeHandle {}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl VolumeHandle {
    pub fn raw(&self) -> HANDLE {
        self.handle
    }
}

#[derive(Debug, Clone)]
pub struct UsnJournalData {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    #[allow(dead_code)]
    pub max_usn: i64,
}

/// Open a raw volume handle for the given drive letter (e.g., 'C').
/// Requires the process to have appropriate privileges (typically admin or backup).
pub fn open_volume(drive_letter: char) -> Result<VolumeHandle, String> {
    let path: Vec<u16> = format!("\\\\.\\{}:", drive_letter)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0x80000000, // GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .map_err(|e| format!("CreateFileW for volume {drive_letter}: failed: {e}"))?
    };

    Ok(VolumeHandle { handle })
}

/// Query the USN journal metadata for the given volume.
pub fn query_usn_journal(vol: &VolumeHandle) -> Result<UsnJournalData, String> {
    // USN_JOURNAL_DATA_V0 layout:
    // UsnJournalID: u64 (8 bytes)
    // FirstUsn: i64 (8 bytes)
    // NextUsn: i64 (8 bytes)
    // LowestValidUsn: i64 (8 bytes)
    // MaxUsn: i64 (8 bytes)
    // MaximumSize: u64 (8 bytes)
    // AllocationDelta: u64 (8 bytes)
    // Total: 56 bytes
    let mut buffer = [0u8; 56];
    let mut bytes_returned: u32 = 0;

    let ok = unsafe {
        DeviceIoControl(
            vol.raw(),
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(buffer.as_mut_ptr() as *mut _),
            buffer.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    if let Err(e) = ok {
        return Err(format!("FSCTL_QUERY_USN_JOURNAL failed: {e}"));
    }

    if (bytes_returned as usize) < 56 {
        return Err(format!(
            "FSCTL_QUERY_USN_JOURNAL returned only {bytes_returned} bytes, expected 56"
        ));
    }

    let journal_id = u64::from_le_bytes(buffer[0..8].try_into().unwrap());
    let first_usn = i64::from_le_bytes(buffer[8..16].try_into().unwrap());
    let next_usn = i64::from_le_bytes(buffer[16..24].try_into().unwrap());
    let max_usn = i64::from_le_bytes(buffer[32..40].try_into().unwrap());

    Ok(UsnJournalData {
        journal_id,
        first_usn,
        next_usn,
        max_usn,
    })
}
