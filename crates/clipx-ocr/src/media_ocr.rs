//! Windows Media OCR（WinRT OcrEngine 直调，ADR-004 的直调替换路径）。
//! 引擎创建优先级对齐 WPF ImageOcrService：用户配置语言 → zh-CN → en-US。

use anyhow::{anyhow, Context, Result};

use crate::postprocess;
use crate::OcrEngine;

pub struct MediaOcrEngine {
    engine: windows::Media::Ocr::OcrEngine,
    /// 引擎支持的最大边长（超限图片先等比缩小再识别，WPF 同款策略）
    max_dim: u32,
}

impl MediaOcrEngine {
    /// 必须在 OCR 工作线程上调用（WinRT 引擎与线程套间绑定）。
    pub fn new() -> Option<MediaOcrEngine> {
        unsafe {
            // 忽略 S_FALSE（本线程已初始化）与已变更套间错误，尽力而为
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );

            let engine = windows::Media::Ocr::OcrEngine::TryCreateFromUserProfileLanguages()
                .or_else(|_| try_create_from_tag("zh-CN"))
                .or_else(|_| try_create_from_tag("en-US"))
                .ok()?;
            let max_dim = windows::Media::Ocr::OcrEngine::MaxImageDimension().unwrap_or(3200).max(512);
            Some(MediaOcrEngine { engine, max_dim })
        }
    }
}

unsafe fn try_create_from_tag(tag: &str) -> windows::core::Result<windows::Media::Ocr::OcrEngine> {
    let lang = windows::Globalization::Language::CreateLanguage(&windows::core::HSTRING::from(tag))?;
    windows::Media::Ocr::OcrEngine::TryCreateFromLanguage(&lang)
}

impl OcrEngine for MediaOcrEngine {
    fn recognize_png(&mut self, png: &[u8]) -> Result<String> {
        // 预缩放：超限大图先缩小（识别精度足够，同时避免 WinRT 解码大图的内存峰值）
        let img = image::load_from_memory(png).context("OCR 图片解码失败")?;
        let img = if img.width().max(img.height()) > self.max_dim {
            img.thumbnail(self.max_dim, self.max_dim)
        } else {
            img
        };
        let mut scaled_png = Vec::new();
        img.to_rgba8()
            .write_to(&mut std::io::Cursor::new(&mut scaled_png), image::ImageFormat::Png)
            .context("OCR 图片重编码失败")?;

        let lines = unsafe { self.recognize_bytes(&scaled_png)? };
        Ok(postprocess::format_result(&lines).unwrap_or_default())
    }
}

impl MediaOcrEngine {
    unsafe fn recognize_bytes(&self, png: &[u8]) -> Result<Vec<Vec<String>>> {
        use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
        use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

        let stream = InMemoryRandomAccessStream::new().context("创建内存流失败")?;
        let writer = DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0)?)
            .context("创建 DataWriter 失败")?;
        writer.WriteBytes(png).context("写入图片字节失败")?;
        // WriteBytes 只进 DataWriter 内部缓冲；StoreAsync 才提交到底层流
        writer
            .StoreAsync()
            .context("提交写入失败")?
            .get()
            .context("提交写入流失败")?;
        let flushed = writer.FlushAsync()?.get().context("刷新写入流失败")?;
        if !flushed {
            return Err(anyhow!("写入流未完全刷新"));
        }
        stream.Seek(0).context("回卷流失败")?;

        let decoder = BitmapDecoder::CreateAsync(&stream)?.get().context("解码图片失败")?;
        let mut bitmap = decoder
            .GetSoftwareBitmapAsync()?
            .get()
            .context("取得 SoftwareBitmap 失败")?;
        // OcrEngine 只认 Bgra8 + Premultiplied
        if bitmap.BitmapPixelFormat()? != BitmapPixelFormat::Bgra8
            || bitmap.BitmapAlphaMode()? != BitmapAlphaMode::Premultiplied
        {
            bitmap = SoftwareBitmap::ConvertWithAlpha(
                &bitmap,
                BitmapPixelFormat::Bgra8,
                BitmapAlphaMode::Premultiplied,
            )
            .context("像素格式转换失败")?;
        }

        let result = self
            .engine
            .RecognizeAsync(&bitmap)?
            .get()
            .context("OCR 识别失败")?;

        let ocr_lines = result.Lines().context("读取 OCR 行失败")?;
        let mut lines = Vec::new();
        for i in 0..ocr_lines.Size()? {
            let line = ocr_lines.GetAt(i)?;
            let words = line.Words()?;
            let mut row = Vec::new();
            for j in 0..words.Size()? {
                row.push(words.GetAt(j)?.Text()?.to_string());
            }
            lines.push(row);
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    /// 端到端引擎冒烟（真机，手动跑）：
    /// PowerShell 生成带字图后：
    ///   $bmp=New-Object Drawing.Bitmap(400,120); ... $bmp.Save("t.png")
    ///   $env:CLIPX_OCR_TEST_PNG="t.png"; cargo test -p clipx-ocr --release media_ocr_e2e -- --ignored --nocapture
    #[test]
    #[ignore]
    fn media_ocr_e2e() {
        use crate::OcrEngine;
        let path = std::env::var("CLIPX_OCR_TEST_PNG").expect("set CLIPX_OCR_TEST_PNG");
        let png = std::fs::read(&path).unwrap();
        let mut engine = super::MediaOcrEngine::new().expect("MediaOcrEngine unavailable");
        let text = engine.recognize_png(&png).unwrap();
        eprintln!("OCR result: {text:?}");
        assert!(!text.trim().is_empty(), "OCR returned empty text");
    }

    /// 分步诊断：定位 WinRT 流/解码器失败环节
    #[test]
    #[ignore]
    fn media_ocr_diag() {
        use windows::Graphics::Imaging::BitmapDecoder;
        use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

        unsafe {
            let hr = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
            eprintln!("RoInitialize: {hr:?}");

            let path = std::env::var("CLIPX_OCR_TEST_PNG").expect("set CLIPX_OCR_TEST_PNG");
            let png = std::fs::read(&path).unwrap();
            eprintln!("png bytes: {}", png.len());

            let stream = InMemoryRandomAccessStream::new().unwrap();
            let writer = DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0).unwrap()).unwrap();
            writer.WriteBytes(&png).unwrap();
            let flushed = writer.FlushAsync().unwrap().get().unwrap();
            eprintln!("flushed: {flushed}, stream size: {}", stream.Size().unwrap());
            stream.Seek(0).unwrap();

            match BitmapDecoder::CreateAsync(&stream) {
                Ok(op) => {
                    eprintln!("CreateAsync ok, pending");
                    match op.get() {
                        Ok(dec) => eprintln!("decoder ok: {:?}", dec.FrameCount()),
                        Err(e) => eprintln!("decoder get err: {e}"),
                    }
                }
                Err(e) => eprintln!("CreateAsync err: {e}"),
            }
        }
    }
}
