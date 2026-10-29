use std::{ffi::OsString, io, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ShellTypeIconKey {
    Directory,
    Extension(OsString),
    GenericFile,
}

impl ShellTypeIconKey {
    pub fn for_entry(path: &Path, directory: bool) -> Self {
        if directory {
            return Self::Directory;
        }
        path.extension()
            .filter(|extension| !extension.is_empty())
            .map(|extension| Self::Extension(lowercase_extension(extension)))
            .unwrap_or(Self::GenericFile)
    }

    pub fn query_path(&self) -> std::path::PathBuf {
        match self {
            Self::Directory => "asterfiles-folder".into(),
            Self::Extension(extension) => {
                let mut name = OsString::from("asterfiles.");
                name.push(extension);
                name.into()
            }
            Self::GenericFile => "asterfiles-file".into(),
        }
    }

    #[cfg(windows)]
    fn lookup(
        &self,
    ) -> (
        OsString,
        windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
    ) {
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        };

        let path = self.query_path().into_os_string();
        let attributes = if matches!(self, Self::Directory) {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        (path, attributes)
    }
}

#[cfg(windows)]
fn lowercase_extension(extension: &std::ffi::OsStr) -> OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let lowered: Vec<u16> = extension
        .encode_wide()
        .map(|unit| {
            if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                unit + u16::from(b'a' - b'A')
            } else {
                unit
            }
        })
        .collect();
    OsString::from_wide(&lowered)
}

#[cfg(not(windows))]
fn lowercase_extension(extension: &std::ffi::OsStr) -> OsString {
    OsString::from(extension.to_string_lossy().to_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIconRgba {
    pub width: u32,
    pub height: u32,
    pub pixels: std::sync::Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbnailSource {
    Cache,
    Provider,
}

#[derive(Debug, Clone)]
pub struct ShellThumbnailRgba {
    pub image: ShellIconRgba,
    pub source: ThumbnailSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridImageSource {
    Thumbnail(ThumbnailSource),
    SystemIcon,
}

#[derive(Debug, Clone)]
pub struct ShellGridImage {
    pub image: ShellIconRgba,
    pub source: GridImageSource,
    pub thumbnail_error: Option<String>,
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
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(Self {
            width,
            height,
            pixels: pixels.into(),
        })
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::{ffi::OsStr, mem::size_of, os::windows::ffi::OsStrExt, ptr};

    use windows::{
        Win32::{
            Foundation::{
                ERROR_FILE_NOT_FOUND, ERROR_NO_ASSOCIATION, ERROR_NOT_FOUND, RPC_E_CHANGED_MODE,
            },
            Graphics::Gdi::{
                BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
                DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
                SelectObject,
            },
            Storage::FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
            },
            System::Ole::{OleInitialize, OleUninitialize},
            UI::{
                Controls::{IImageList, ILD_TRANSPARENT},
                Shell::{
                    ASSOCF_NOTRUNCATE, ASSOCSTR_SHELLEXTENSION, AssocQueryStringW,
                    IShellItemImageFactory, SHCreateItemFromParsingName, SHFILEINFOW, SHGFI_ICON,
                    SHGFI_LARGEICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
                    SHGetImageList, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SIIGBF_BIGGERSIZEOK,
                    SIIGBF_ICONONLY, SIIGBF_INCACHEONLY, SIIGBF_SCALEUP, SIIGBF_THUMBNAILONLY,
                },
                WindowsAndMessaging::{
                    DI_NORMAL, DestroyIcon, DrawIconEx, GetSystemMetrics, HICON, SM_CXICON,
                    SM_CYICON,
                },
            },
        },
        core::{HRESULT, PCWSTR, PWSTR},
    };

    use super::{
        GridImageSource, Path, ShellGridImage, ShellIconRgba, ShellThumbnailRgba, ShellTypeIconKey,
        ThumbnailSource, io,
    };

    pub struct ShellWorkerApartment {
        _com: ComInitialization,
    }

    pub fn initialize_shell_worker() -> io::Result<ShellWorkerApartment> {
        ComInitialization::new().map(|com| ShellWorkerApartment { _com: com })
    }

    pub fn shell_icon_rgba(path: &Path) -> io::Result<ShellIconRgba> {
        let icon = ShellIcon::for_path(path)?;
        icon.to_rgba()
    }

    pub fn shell_icon_rgba_at_size(path: &Path, size: u32) -> io::Result<ShellIconRgba> {
        use windows::Win32::Foundation::SIZE;
        let size = i32::try_from(size)
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid Shell icon size")
            })?;
        let wide = wide_null(path.as_os_str());
        let factory: IShellItemImageFactory = unsafe {
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(windows_error)?
        };
        // Ask Shell for the file's own image at the requested size without enlarging a small icon.
        let bitmap = unsafe {
            factory
                .GetImage(
                    SIZE { cx: size, cy: size },
                    SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
                )
                .map_err(windows_error)?
        };
        owned_bitmap_to_rgba(bitmap)
    }

    pub fn shell_grid_image_rgba(path: &Path, size: u32) -> io::Result<ShellGridImage> {
        let thumbnail = shell_thumbnail_rgba(path, size, true)
            .ok()
            .filter(|thumbnail| thumbnail.image.width.max(thumbnail.image.height) >= size)
            .map(Ok)
            .unwrap_or_else(|| shell_thumbnail_rgba(path, size, false));
        match thumbnail {
            Ok(thumbnail) => Ok(ShellGridImage {
                image: thumbnail.image,
                source: GridImageSource::Thumbnail(thumbnail.source),
                thumbnail_error: None,
            }),
            Err(thumbnail_error) => {
                let image = shell_icon_rgba_at_size(path, size).map_err(|icon_error| {
                    io::Error::other(format!(
                        "Shell thumbnail unavailable: {thumbnail_error}; system icon unavailable: {icon_error}"
                    ))
                })?;
                Ok(ShellGridImage {
                    image,
                    source: GridImageSource::SystemIcon,
                    thumbnail_error: Some(thumbnail_error.to_string()),
                })
            }
        }
    }
    pub fn shell_type_icon_rgba(key: &ShellTypeIconKey) -> io::Result<ShellIconRgba> {
        let (lookup_name, attributes) = key.lookup();
        let wide_name = wide_null(&lookup_name);
        let mut file_info = SHFILEINFOW::default();
        let result = unsafe {
            SHGetFileInfoW(
                PCWSTR(wide_name.as_ptr()),
                attributes,
                Some(&mut file_info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
            )
        };
        if result == 0 || file_info.hIcon.is_invalid() {
            return Err(io::Error::other("Windows Shell type icon unavailable"));
        }
        ShellIcon(file_info.hIcon).to_rgba()
    }

    /// Resolves a type's shared image-list icon without opening any directory entry.
    pub fn shell_type_icon_rgba_at_size(
        key: &ShellTypeIconKey,
        size: u32,
    ) -> io::Result<ShellIconRgba> {
        let size = i32::try_from(size)
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid Shell icon size")
            })?;
        let (lookup_name, attributes) = key.lookup();
        let wide_name = wide_null(&lookup_name);
        let mut file_info = SHFILEINFOW::default();
        let result = unsafe {
            SHGetFileInfoW(
                PCWSTR(wide_name.as_ptr()),
                attributes,
                Some(&mut file_info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES,
            )
        };
        if result == 0 {
            return Err(io::Error::other(
                "Windows Shell type icon index unavailable",
            ));
        }
        for list_id in [SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO] {
            let images: IImageList =
                unsafe { SHGetImageList(list_id as i32) }.map_err(windows_error)?;
            let (mut width, mut height) = (0, 0);
            unsafe { images.GetIconSize(&mut width, &mut height) }.map_err(windows_error)?;
            if width <= 0 || height <= 0 {
                return Err(io::Error::other(
                    "Windows Shell image list has invalid dimensions",
                ));
            }
            if width.min(height) >= size || list_id == SHIL_JUMBO {
                let icon = unsafe { images.GetIcon(file_info.iIcon, ILD_TRANSPARENT.0) }
                    .map_err(windows_error)?;
                // Downsample a native larger image; never manufacture detail by scaling up.
                return ShellIcon(icon).to_rgba_at_size(size.min(width), size.min(height));
            }
        }
        unreachable!("the jumbo list terminates the image-list search")
    }

    /// Reads installed association handlers, not the success of one particular file's preview.
    pub fn supports_content_thumbnail(key: &ShellTypeIconKey) -> io::Result<bool> {
        let association = match key {
            ShellTypeIconKey::Directory => return Ok(true),
            ShellTypeIconKey::Extension(extension) => {
                let mut association = std::ffi::OsString::from(".");
                association.push(extension);
                association
            }
            ShellTypeIconKey::GenericFile => "*".into(),
        };
        let association = wide_null(&association);
        // Windows still accepts legacy IExtractImage registrations alongside IThumbnailProvider.
        for handler in [
            "{E357FCCD-A995-4576-B01F-234630154E96}",
            "{BB2E617C-0920-11D1-9A0B-00C04FC2D6C1}",
        ] {
            let handler = wide_null(OsStr::new(handler));
            let mut output = [0_u16; 128];
            let mut length = output.len() as u32;
            let result = unsafe {
                AssocQueryStringW(
                    ASSOCF_NOTRUNCATE,
                    ASSOCSTR_SHELLEXTENSION,
                    PCWSTR(association.as_ptr()),
                    PCWSTR(handler.as_ptr()),
                    Some(PWSTR(output.as_mut_ptr())),
                    &mut length,
                )
            };
            if result.is_ok() {
                if output[0] != 0 {
                    return Ok(true);
                }
            } else if ![ERROR_FILE_NOT_FOUND, ERROR_NOT_FOUND, ERROR_NO_ASSOCIATION]
                .iter()
                .any(|error| result == HRESULT::from_win32(error.0))
            {
                return Err(io::Error::other(format!(
                    "Windows Shell thumbnail association query failed: {result:?}"
                )));
            }
        }
        Ok(false)
    }

    pub fn shell_thumbnail_rgba(
        path: &Path,
        size: u32,
        cache_only: bool,
    ) -> io::Result<ShellThumbnailRgba> {
        use windows::Win32::Foundation::SIZE;
        let wide = wide_null(path.as_os_str());
        let factory: IShellItemImageFactory = unsafe {
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(windows_error)?
        };
        let flags = if cache_only {
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK | SIIGBF_INCACHEONLY
        } else {
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK | SIIGBF_SCALEUP
        };
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
        owned_bitmap_to_rgba(bitmap).map(|image| ShellThumbnailRgba {
            image,
            source: if cache_only {
                ThumbnailSource::Cache
            } else {
                ThumbnailSource::Provider
            },
        })
    }

    fn owned_bitmap_to_rgba(bitmap: HBITMAP) -> io::Result<ShellIconRgba> {
        let bitmap = Bitmap(bitmap);
        bitmap_to_rgba(bitmap.0)
    }

    fn bitmap_to_rgba(bitmap: HBITMAP) -> io::Result<ShellIconRgba> {
        use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
        let mut native = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                bitmap.into(),
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
                bitmap,
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
            let result = unsafe { OleInitialize(None) };
            if result.is_ok() {
                Ok(Self { uninitialize: true })
            } else if result
                .as_ref()
                .is_err_and(|error| error.code() == RPC_E_CHANGED_MODE)
            {
                Ok(Self {
                    uninitialize: false,
                })
            } else {
                Err(io::Error::other(format!(
                    "OleInitialize failed: {result:?}"
                )))
            }
        }
    }

    impl Drop for ComInitialization {
        fn drop(&mut self) {
            if self.uninitialize {
                unsafe { OleUninitialize() };
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

            self.to_rgba_at_size(width, height)
        }

        fn to_rgba_at_size(&self, width: i32, height: i32) -> io::Result<ShellIconRgba> {
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
pub use windows_impl::{
    initialize_shell_worker, shell_grid_image_rgba, shell_icon_rgba, shell_icon_rgba_at_size,
    shell_type_icon_rgba, shell_type_icon_rgba_at_size, supports_content_thumbnail,
};

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ShellIconRgba, ShellTypeIconKey};

    #[test]
    fn type_icon_keys_match_for_local_and_unc_entries() {
        for extension in ["blend", "fbx", "obj"] {
            assert_eq!(
                ShellTypeIconKey::for_entry(
                    Path::new(&format!(r"C:\local\asset.{extension}")),
                    false,
                ),
                ShellTypeIconKey::for_entry(
                    Path::new(&format!(r"\\server\share\asset.{extension}")),
                    false,
                ),
            );
        }
        assert_eq!(
            ShellTypeIconKey::for_entry(Path::new(r"C:\local\asset.BLEND"), false),
            ShellTypeIconKey::for_entry(Path::new(r"\\server\share\asset.blend"), false),
        );
        assert_eq!(
            ShellTypeIconKey::for_entry(Path::new("README"), false),
            ShellTypeIconKey::GenericFile
        );
        assert_eq!(
            ShellTypeIconKey::for_entry(Path::new("folder.txt"), true),
            ShellTypeIconKey::Directory
        );
    }

    #[cfg(windows)]
    #[test]
    fn issue_98_sized_icon_rejects_invalid_dimensions_before_shell_access() {
        for size in [0, i32::MAX as u32 + 1, u32::MAX] {
            let error = super::shell_icon_rgba_at_size(Path::new("nonexistent"), size).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }
    #[cfg(windows)]
    #[test]
    fn issue_175_shared_type_icon_sizes_and_registered_preview_capabilities() {
        let _apartment = super::initialize_shell_worker().unwrap();
        for extension in ["txt", "png"] {
            let key = ShellTypeIconKey::Extension(extension.into());
            let supported = super::supports_content_thumbnail(&key).unwrap();
            eprintln!("#175 extension={extension} registered_thumbnail_provider={supported}");
            for size in [32, 48, 128, 256] {
                let image = super::shell_type_icon_rgba_at_size(&key, size).unwrap();
                assert!(image.width > 0 && image.width <= size);
                assert!(image.height > 0 && image.height <= size);
                assert_eq!(
                    image.pixels.len(),
                    (image.width * image.height * 4) as usize
                );
                eprintln!(
                    "#175 extension={extension} requested_px={size} shared_icon={}x{}",
                    image.width, image.height
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn issue_175_shared_type_icon_rejects_invalid_dimensions_without_shell_access() {
        for size in [0, i32::MAX as u32 + 1, u32::MAX] {
            let error = super::shell_type_icon_rgba_at_size(&ShellTypeIconKey::GenericFile, size)
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn converts_bgra_pixels_to_rgba() {
        let icon = ShellIconRgba::from_bgra(2, 1, vec![1, 2, 3, 4, 10, 20, 30, 40]).unwrap();

        assert_eq!(icon.pixels.as_ref(), &[3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn rejects_an_invalid_pixel_buffer_length() {
        let error = ShellIconRgba::from_bgra(2, 2, vec![0; 4]).unwrap_err();

        assert!(error.to_string().contains("expected 16"));
    }
}
