//! Windows WIC 边解边缩：预览/缩略图热路径用系统解码器，不解出全分辨率再缩。
//!
//! 失败回 None，调用方走 `image` crate。COM 在调用线程 MTA 初始化（已初始化则忽略）。

use std::cell::RefCell;
use std::path::Path;

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoder, IWICBitmapSource,
    IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone, WICBitmapInterpolationModeLinear,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

pub struct DecodedRgba {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

thread_local! {
    static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

/// 解码并限制长边 ≤ `max_dim`（只缩不放），输出非预乘 RGBA。
pub fn decode_limited(bytes: &[u8], max_dim: u32) -> Option<DecodedRgba> {
    if bytes.is_empty() || max_dim == 0 {
        return None;
    }
    with_factory(|factory| unsafe {
        let stream = factory.CreateStream().ok()?;
        stream.InitializeFromMemory(bytes).ok()?;
        let decoder = factory
            .CreateDecoderFromStream(&*stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        pixels_from_decoder(factory, &decoder, max_dim)
    })
}

/// 从文件路径边解边缩（不把整文件读进内存）。多图文件预览用。
pub fn decode_limited_path(path: &str, max_dim: u32) -> Option<DecodedRgba> {
    if path.is_empty() || max_dim == 0 || !Path::new(path).is_file() {
        return None;
    }
    with_factory(|factory| unsafe {
        let decoder = factory
            .CreateDecoderFromFilename(
                &HSTRING::from(path),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;
        pixels_from_decoder(factory, &decoder, max_dim)
    })
}

fn with_factory<T>(f: impl FnOnce(&IWICImagingFactory) -> Option<T>) -> Option<T> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    FACTORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let fac = unsafe {
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            }
            .ok()?;
            *slot = Some(fac);
        }
        f(slot.as_ref().unwrap())
    })
}

unsafe fn pixels_from_decoder(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    max_dim: u32,
) -> Option<DecodedRgba> {
    let frame = decoder.GetFrame(0).ok()?;
    let mut src_w = 0u32;
    let mut src_h = 0u32;
    frame.GetSize(&mut src_w, &mut src_h).ok()?;
    if src_w == 0 || src_h == 0 {
        return None;
    }
    let (dst_w, dst_h) = fit_decode_width(src_w, src_h, max_dim);

    let source: IWICBitmapSource = if dst_w != src_w || dst_h != src_h {
        let scaler = factory.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(&frame, dst_w, dst_h, WICBitmapInterpolationModeLinear)
            .ok()?;
        scaler.cast().ok()?
    } else {
        frame.cast().ok()?
    };

    let converter = factory.CreateFormatConverter().ok()?;
    converter
        .Initialize(
            &source,
            &GUID_WICPixelFormat32bppRGBA,
            WICBitmapDitherTypeNone,
            None::<&IWICPalette>,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;

    let stride = dst_w.checked_mul(4)?;
    let nbytes = (stride as usize).checked_mul(dst_h as usize)?;
    let mut buf = vec![0u8; nbytes];
    converter
        .CopyPixels(std::ptr::null(), stride, &mut buf)
        .ok()?;

    Some(DecodedRgba {
        rgba: buf,
        w: dst_w,
        h: dst_h,
    })
}

/// 对齐 WPF DecodePixelWidth：只限宽度，高度按比例（4K 横图 → 520×293）。
fn fit_decode_width(w: u32, h: u32, max_w: u32) -> (u32, u32) {
    if w <= max_w || w == 0 {
        return (w, h);
    }
    let nh = ((h as u64 * max_w as u64) / w as u64).max(1) as u32;
    (max_w, nh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 10, 30]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn wic_decodes_png_and_downscales() {
        let png = tiny_png(320, 120);
        let full = decode_limited(&png, 1600).expect("wic decode");
        assert_eq!((full.w, full.h), (320, 120));
        assert_eq!(full.rgba.len(), 320 * 120 * 4);
        assert_eq!(&full.rgba[0..4], &[200, 10, 30, 255]);

        let mid = decode_limited(&png, 64).expect("wic scale");
        assert_eq!(mid.w, 64);
        assert_eq!(mid.h, 24);
        assert_eq!(mid.rgba.len(), 64 * 24 * 4);
    }

    #[test]
    fn wic_rejects_garbage() {
        assert!(decode_limited(&[], 64).is_none());
        assert!(decode_limited(&[0, 1, 2, 3], 64).is_none());
        assert!(decode_limited_path("", 64).is_none());
        assert!(decode_limited_path("C:\\clipx-no-such-file.png", 64).is_none());
    }

    #[test]
    fn wic_decodes_from_path_without_full_read() {
        let png = tiny_png(80, 40);
        let dir = std::env::temp_dir().join("clipx-wic-path-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tiny.png");
        std::fs::write(&path, &png).unwrap();
        let d = decode_limited_path(&path.to_string_lossy(), 1600).expect("path decode");
        assert_eq!((d.w, d.h), (80, 40));
        let small = decode_limited_path(&path.to_string_lossy(), 40).expect("path scale");
        assert_eq!((small.w, small.h), (40, 20));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
