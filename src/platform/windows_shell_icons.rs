use std::{io, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIconRgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl ShellIconRgba {
    fn from_bgra(width: u32, height: u32, pixels: Vec<u8>) -> io::Result<Self> {
        let expected_len = width
            .checked_mul(height)
            .and_then(|pixel_count| pixel_count.checked_mul(4))
            .and_then(|byte_count| usize::try_from(byte_count).ok())
            .ok_or_else(|| io::Error::other("Shell icon dimensions overflow"))?;
        if pixels.len() != expected_len {
            return Err(io::Error::other(format!(
                "Shell icon pixel buffer has {} bytes, expected {expected_len}",
                pixels.len()
            )));
        }

        let mut pixels = pixels;
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{ffi::OsStr, mem::size_of, os::windows::ffi::OsStrExt, ptr};

    use windows::{
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
            Graphics::Gdi::{
                BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
                DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
                SelectObject,
            },
            Storage::FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
            },
            System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
            UI::{
                Shell::{
                    IShellItemImageFactory, SHCreateItemFromParsingName, SHFILEINFOW, SHGFI_ICON,
                    SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SIIGBF_BIGGERSIZEOK,
                    SIIGBF_INCACHEONLY, SIIGBF_THUMBNAILONLY,
                },
                WindowsAndMessaging::{
                    DI_NORMAL, DestroyIcon, DrawIconEx, GetSystemMetrics, HICON, SM_CXICON,
                    SM_CYICON,
                },
            },
        },
        core::PCWSTR,
    };

    use super::{Path, ShellIconRgba, io};

    pub fn shell_icon_rgba(path: &Path) -> io::Result<ShellIconRgba> {
        let _com = ComInitialization::new()?;
        let icon = ShellIcon::for_path(path)?;
        icon.to_rgba()
    }

    pub fn shell_thumbnail_rgba(
        path: &Path,
        size: u32,
        cache_only: bool,
    ) -> io::Result<ShellIconRgba> {
        use windows::Win32::{
            Foundation::SIZE,
            Graphics::Gdi::{GetDC, ReleaseDC},
        };
        let _com = ComInitialization::new()?;
        let wide = wide_null(path.as_os_str());
        let factory: IShellItemImageFactory = unsafe {
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(windows_error)?
        };
        let mut flags = SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK;
        if cache_only {
            flags |= SIIGBF_INCACHEONLY;
        }
        let bitmap = unsafe {
            factory
                .GetImage(
                    SIZE {
                        cx: size as i32,
                        cy: size as i32,
                    },
                    flags,
                )
                .map_err(windows_error)?
        };
        let bitmap = Bitmap(bitmap);
        let mut native = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                bitmap.0.into(),
                size_of::<BITMAP>() as i32,
                Some((&mut native as *mut BITMAP).cast()),
            )
        };
        if read == 0 || native.bmWidth <= 0 || native.bmHeight == 0 {
            return Err(io::Error::last_os_error());
        }
        let width = native.bmWidth as u32;
        let height = native.bmHeight.unsigned_abs();
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0; width as usize * height as usize * 4];
        let dc = unsafe { GetDC(None) };
        if dc.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        let lines = unsafe {
            GetDIBits(
                dc,
                bitmap.0,
                0,
                height,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        unsafe { ReleaseDC(None, dc) };
        if lines != height as i32 {
            return Err(io::Error::other(
                "Windows Shell thumbnail pixels unavailable",
            ));
        }
        ShellIconRgba::from_bgra(width, height, pixels)
    }

    struct ComInitialization {
        uninitialize: bool,
    }

    impl ComInitialization {
        fn new() -> io::Result<Self> {
            let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if result.is_ok() {
                Ok(Self { uninitialize: true })
            } else if result == RPC_E_CHANGED_MODE {
                Ok(Self {
                    uninitialize: false,
                })
            } else {
                Err(io::Error::other(format!(
                    "CoInitializeEx failed: {result:?}"
                )))
            }
        }
    }

    impl Drop for ComInitialization {
        fn drop(&mut self) {
            if self.uninitialize {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct ShellIcon(HICON);

    impl ShellIcon {
        fn for_path(path: &Path) -> io::Result<Self> {
            let wide_path = wide_null(path.as_os_str());
            let mut file_info = SHFILEINFOW::default();
            let flags = SHGFI_ICON | SHGFI_LARGEICON;
            let result = unsafe {
                SHGetFileInfoW(
                    PCWSTR(wide_path.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES::default(),
                    Some(&mut file_info),
                    size_of::<SHFILEINFOW>() as u32,
                    flags,
                )
            };
            if result != 0 && !file_info.hIcon.is_invalid() {
                return Ok(Self(file_info.hIcon));
            }

            let result = unsafe {
                SHGetFileInfoW(
                    PCWSTR(wide_path.as_ptr()),
                    fallback_attributes(path),
                    Some(&mut file_info),
                    size_of::<SHFILEINFOW>() as u32,
                    flags | SHGFI_USEFILEATTRIBUTES,
                )
            };
            if result == 0 || file_info.hIcon.is_invalid() {
                return Err(io::Error::other(format!(
                    "SHGetFileInfoW could not obtain an icon for {}",
                    path.display()
                )));
            }
            Ok(Self(file_info.hIcon))
        }

        fn to_rgba(&self) -> io::Result<ShellIconRgba> {
            let width = unsafe { GetSystemMetrics(SM_CXICON) };
            let height = unsafe { GetSystemMetrics(SM_CYICON) };
            if width <= 0 || height <= 0 {
                return Err(io::Error::other(format!(
                    "GetSystemMetrics returned invalid icon dimensions {width}x{height}"
                )));
            }

            let byte_len = usize::try_from(width)
                .ok()
                .and_then(|width| {
                    usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixel_count| pixel_count.checked_mul(4))
                .ok_or_else(|| io::Error::other("Shell icon dimensions overflow"))?;

            let bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixel_pointer = ptr::null_mut();
            let dc = unsafe { CreateCompatibleDC(None) };
            if dc.is_invalid() {
                return Err(io::Error::last_os_error());
            }
            let dc = DeviceContext(dc);
            let bitmap = unsafe {
                CreateDIBSection(
                    Some(dc.0),
                    &bitmap_info,
                    DIB_RGB_COLORS,
                    &mut pixel_pointer,
                    None,
                    0,
                )
            }
            .map_err(windows_error)?;
            if pixel_pointer.is_null() {
                let _ = unsafe { DeleteObject(bitmap.into()) };
                return Err(io::Error::other(
                    "CreateDIBSection returned no pixel buffer",
                ));
            }
            let bitmap = Bitmap(bitmap);
            let previous = unsafe { SelectObject(dc.0, bitmap.0.into()) };
            if previous.is_invalid() {
                return Err(io::Error::last_os_error());
            }
            let selection = BitmapSelection { dc: dc.0, previous };

            unsafe { ptr::write_bytes(pixel_pointer.cast::<u8>(), 0, byte_len) };
            unsafe { DrawIconEx(dc.0, 0, 0, self.0, width, height, 0, None, DI_NORMAL) }
                .map_err(windows_error)?;
            let pixels = unsafe {
                std::slice::from_raw_parts(pixel_pointer.cast::<u8>(), byte_len).to_vec()
            };
            drop(selection);
            drop(bitmap);
            drop(dc);

            ShellIconRgba::from_bgra(width as u32, height as u32, pixels)
        }
    }

    impl Drop for ShellIcon {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { DestroyIcon(self.0) };
            }
        }
    }

    struct DeviceContext(windows::Win32::Graphics::Gdi::HDC);

    impl Drop for DeviceContext {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { DeleteDC(self.0) };
            }
        }
    }

    struct Bitmap(HBITMAP);

    impl Drop for Bitmap {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { DeleteObject(self.0.into()) };
            }
        }
    }

    struct BitmapSelection {
        dc: windows::Win32::Graphics::Gdi::HDC,
        previous: HGDIOBJ,
    }

    impl Drop for BitmapSelection {
        fn drop(&mut self) {
            if !self.dc.is_invalid() && !self.previous.is_invalid() {
                unsafe { SelectObject(self.dc, self.previous) };
            }
        }
    }

    fn fallback_attributes(path: &Path) -> FILE_FLAGS_AND_ATTRIBUTES {
        if path.extension().is_none() || path.as_os_str().to_string_lossy().ends_with(['\\', '/']) {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        }
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn windows_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }
}

#[cfg(windows)]
pub use windows_impl::{shell_icon_rgba, shell_thumbnail_rgba};

#[cfg(not(windows))]
pub fn shell_icon_rgba(_path: &Path) -> io::Result<ShellIconRgba> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows Shell icons are only available on Windows",
    ))
}

#[cfg(not(windows))]
pub fn shell_thumbnail_rgba(
    _path: &Path,
    _size: u32,
    _cache_only: bool,
) -> io::Result<ShellIconRgba> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows Shell thumbnails are only available on Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::ShellIconRgba;

    #[test]
    fn converts_bgra_pixels_to_rgba() {
        let icon = ShellIconRgba::from_bgra(2, 1, vec![1, 2, 3, 4, 10, 20, 30, 40]).unwrap();

        assert_eq!(icon.pixels, vec![3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn rejects_an_invalid_pixel_buffer_length() {
        let error = ShellIconRgba::from_bgra(2, 2, vec![0; 4]).unwrap_err();

        assert!(error.to_string().contains("expected 16"));
    }
}
