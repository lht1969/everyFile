use base64::Engine;
use std::collections::HashMap;
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

lazy_static::lazy_static! {
    static ref ICON_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
}

#[tauri::command]
pub async fn get_file_icon(file_path: String, is_directory: bool) -> Result<String, String> {
    let key = if is_directory {
        "__directory__".to_string()
    } else {
        std::path::Path::new(&file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_else(|| "__noext__".to_string())
    };

    {
        let cache = ICON_CACHE.lock().map_err(|e| e.to_string())?;
        if let Some(icon) = cache.get(&key) {
            return Ok(icon.clone());
        }
    }

    let key_clone = key.clone();
    let icon_data = tokio::task::spawn_blocking(move || extract_icon_base64(&key_clone, is_directory))
        .await
        .map_err(|e| e.to_string())??;

    {
        let mut cache = ICON_CACHE.lock().map_err(|e| e.to_string())?;
        cache.insert(key, icon_data.clone());
    }

    Ok(icon_data)
}

fn extract_icon_base64(key: &str, is_directory: bool) -> Result<String, String> {
    let test_path = if is_directory {
        "__folder__".to_string()
    } else {
        format!("file{}", key)
    };

    let path_wide: Vec<u16> = test_path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut file_info = SHFILEINFOW::default();
        let flags = SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES;

        let result = SHGetFileInfoW(
            windows::core::PCWSTR(path_wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut file_info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );

        if result == 0 || file_info.hIcon.is_invalid() {
            return Err("Failed to get file icon".to_string());
        }

        let icon = file_info.hIcon;
        let width = 16i32;
        let height = 16i32;

        let hdc = CreateCompatibleDC(HDC::default());
        if hdc.is_invalid() {
            let _ = DestroyIcon(icon);
            return Err("Failed to create DC".to_string());
        }

        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = width;
        bi.bmiHeader.biHeight = -height;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp_result = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0);
        if hbmp_result.is_err() || bits.is_null() {
            let _ = DeleteDC(hdc);
            let _ = DestroyIcon(icon);
            return Err("Failed to create DIB section".to_string());
        }
        let hbmp = hbmp_result.unwrap();

        let old = SelectObject(hdc, std::mem::transmute::<HBITMAP, HGDIOBJ>(hbmp));

        let _ = DrawIconEx(hdc, 0, 0, icon, width, height, 0, None, DI_NORMAL);

        let pixel_count = (width * height) as usize;
        let mut pixels = vec![0u8; pixel_count * 4];
        std::ptr::copy_nonoverlapping(bits as *const u8, pixels.as_mut_ptr(), pixel_count * 4);

        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc);
        let _ = DestroyIcon(icon);

        let bmp_data = encode_bmp(&pixels, width as u32, height as u32)?;

        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(&bmp_data);
        Ok(format!("data:image/bmp;base64,{}", b64))
    }
}

fn encode_bmp(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let row_size = ((width * 32 + 31) / 32) * 4;
    let pixel_data_size = row_size * height;
    let header_size = 14u32 + 40u32;
    let file_size = header_size + pixel_data_size;

    let mut data = Vec::with_capacity(file_size as usize);

    data.extend_from_slice(b"BM");
    data.extend_from_slice(&file_size.to_le_bytes());
    data.extend_from_slice(&[0u8; 4]);
    data.extend_from_slice(&header_size.to_le_bytes());

    data.extend_from_slice(&(40u32).to_le_bytes());
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&(1u16).to_le_bytes());
    data.extend_from_slice(&(32u16).to_le_bytes());
    data.extend_from_slice(&(0u32).to_le_bytes());
    data.extend_from_slice(&pixel_data_size.to_le_bytes());
    data.extend_from_slice(&[0u8; 4]);
    data.extend_from_slice(&[0u8; 4]);
    data.extend_from_slice(&(0u32).to_le_bytes());
    data.extend_from_slice(&(0u32).to_le_bytes());

    let row_bytes = (width * 4) as usize;
    for y in (0..height).rev() {
        let start = (y as usize) * row_bytes;
        let end = start + row_bytes;
        if end > pixels.len() {
            return Err("Pixel data too short".to_string());
        }
        data.extend_from_slice(&pixels[start..end]);
        let padding = (row_size - width * 4) as usize;
        data.extend(std::iter::repeat(0u8).take(padding));
    }

    Ok(data)
}
