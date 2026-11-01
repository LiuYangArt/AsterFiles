use std::{io, os::windows::ffi::OsStrExt, path::Path};

use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::PCWSTR};

pub const LOW_SPACE_PERCENT: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveCapacity {
    pub total: u64,
    pub available: u64,
}

impl DriveCapacity {
    pub fn used_ratio(self) -> f32 {
        used_ratio(self.total, self.available)
    }

    pub fn is_low_space(self) -> bool {
        is_low_space(self.total, self.available)
    }
}

pub fn query_capacity(root: &Path) -> io::Result<DriveCapacity> {
    let root = root
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut available = 0;
    let mut total = 0;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            None,
        )
    }
    .map_err(io::Error::other)?;
    Ok(DriveCapacity { total, available })
}

pub fn used_ratio(total: u64, available: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    let used_bytes = total.saturating_sub(available.min(total));
    (used_bytes as f64 / total as f64).clamp(0.0, 1.0) as f32
}

pub fn is_low_space(total: u64, available: u64) -> bool {
    total != 0
        && u128::from(available.min(total)) * 100
            < u128::from(total) * u128::from(LOW_SPACE_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_ratio_uses_available_capacity_for_the_current_user() {
        assert_eq!(used_ratio(1_000, 250), 0.75);
    }

    #[test]
    fn zero_total_capacity_is_empty_without_dividing_by_zero() {
        assert_eq!(used_ratio(0, 0), 0.0);
        assert!(!is_low_space(0, 0));
    }

    #[test]
    fn available_capacity_above_total_does_not_underflow() {
        assert_eq!(used_ratio(100, 200), 0.0);
        assert!(!is_low_space(100, 200));
    }

    #[test]
    fn low_space_threshold_is_strictly_below_ten_percent() {
        assert!(!is_low_space(1_000, 100));
        assert!(is_low_space(1_000, 99));
    }

    #[test]
    fn query_reports_windows_errors() {
        assert!(query_capacity(Path::new(r"?:\")).is_err());
    }
    #[test]
    fn threshold_calculation_does_not_overflow_u64() {
        assert!(!is_low_space(u64::MAX, u64::MAX / 10 + 1));
        assert!(is_low_space(u64::MAX, u64::MAX / 10 - 1));
    }
}
