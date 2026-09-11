//! Windows Media OCR（WinRT OcrEngine 直调，ADR-004 的直调替换路径）。
//! 引擎创建优先级对齐 WPF ImageOcrService：用户配置语言 → zh-CN → en-US。

use anyhow::{anyhow, Context, Result};

use crate::postprocess;
use crate::{OcrDetailed, OcrEngine, OcrLine, OcrWord};

/// 单图词框上限：超长截图只保留前 1200 个词（存量 ~60KB，懒加载可接受）。
const WORD_CAP: usize = 1200;

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
            let max_dim = windows::Media::Ocr::OcrEngine::MaxImageDimension()
                .unwrap_or(3200)
                .max(512);
            Some(MediaOcrEngine { engine, max_dim })
        }
    }
}

unsafe fn try_create_from_tag(tag: &str) -> windows::core::Result<windows::Media::Ocr::OcrEngine> {
    let lang =
        windows::Globalization::Language::CreateLanguage(&windows::core::HSTRING::from(tag))?;
    windows::Media::Ocr::OcrEngine::TryCreateFromLanguage(&lang)
}

impl OcrEngine for MediaOcrEngine {
    fn recognize_png(&mut self, png: &[u8]) -> Result<String> {
        Ok(self.recognize_detailed_inner(png)?.text)
    }

    fn recognize_lines(&mut self, png: &[u8]) -> Result<OcrDetailed> {
        self.recognize_detailed_inner(png)
    }
}

impl MediaOcrEngine {
    fn recognize_detailed_inner(&self, png: &[u8]) -> Result<OcrDetailed> {
        // 预缩放：超限大图先缩小（识别精度足够，同时避免 WinRT 解码大图的内存峰值）
        let img = image::load_from_memory(png).context("OCR 图片解码失败")?;
        let img = if img.width().max(img.height()) > self.max_dim {
            img.thumbnail(self.max_dim, self.max_dim)
        } else {
            img
        };
        let mut scaled_png = Vec::new();
        img.to_rgba8()
            .write_to(
                &mut std::io::Cursor::new(&mut scaled_png),
                image::ImageFormat::Png,
            )
            .context("OCR 图片重编码失败")?;

        let raw = unsafe { self.recognize_bytes_detailed(&scaled_png)? };
        let word_lines: Vec<Vec<String>> =
            raw.lines.iter().map(|l| l.words.clone()).collect();
        let text = postprocess::format_result(&word_lines).unwrap_or_default();
        // 行文本与全文走同一后处理（单行调 format_result），框取词框并集后归一化。
        // 词框逐词归一化保留（P1b-2 图上选词；中文词多为 1-3 字，约等于按字选）。
        let mut lines = Vec::with_capacity(raw.lines.len());
        let mut word_total = 0;
        'rows: for l in &raw.lines {
            if l.words.iter().all(|w| w.trim().is_empty()) {
                continue;
            }
            let line_text =
                postprocess::format_result(&[l.words.clone()]).unwrap_or_default();
            if line_text.trim().is_empty() {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) =
                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for r in &l.rects {
                x0 = x0.min(r.x);
                y0 = y0.min(r.y);
                x1 = x1.max(r.x + r.w);
                y1 = y1.max(r.y + r.h);
            }
            if x1 <= x0 || y1 <= y0 || raw.w == 0 || raw.h == 0 {
                continue;
            }
            let mut words = Vec::with_capacity(l.words.len());
            for (wt, r) in l.words.iter().zip(l.rects.iter()) {
                if wt.trim().is_empty() {
                    continue;
                }
                if word_total >= WORD_CAP {
                    break 'rows;
                }
                word_total += 1;
                words.push(
                    OcrWord {
                        text: wt.clone(),
                        x: r.x / raw.w as f32,
                        y: r.y / raw.h as f32,
                        w: r.w / raw.w as f32,
                        h: r.h / raw.h as f32,
                    }
                    .clamped(),
                );
            }
            lines.push(
                OcrLine {
                    text: line_text,
                    x: x0 / raw.w as f32,
                    y: y0 / raw.h as f32,
                    w: (x1 - x0) / raw.w as f32,
                    h: (y1 - y0) / raw.h as f32,
                    words,
                }
                .clamped(),
            );
            // 单图行数上限：超长截图行框只保留前 200 行（预览够用）。
            if lines.len() >= 200 {
                break;
            }
        }
        Ok(OcrDetailed { text, lines })
    }
}

impl MediaOcrEngine {
    /// 原始行识别：词文本 + 词框（像素坐标）+ 识别用位图尺寸（归一化用）。
    unsafe fn recognize_bytes_detailed(&self, png: &[u8]) -> Result<RawOcr> {
        use windows::Graphics::Imaging::{
            BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, SoftwareBitmap,
        };
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

        let decoder = BitmapDecoder::CreateAsync(&stream)?
            .get()
            .context("解码图片失败")?;
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
        let w = bitmap.PixelWidth().context("读位图宽度失败")? as u32;
        let h = bitmap.PixelHeight().context("读位图高度失败")? as u32;
        let mut lines = Vec::new();
        for i in 0..ocr_lines.Size()? {
            let line = ocr_lines.GetAt(i)?;
            let words = line.Words()?;
            let mut row = RawLine {
                words: Vec::new(),
                rects: Vec::new(),
            };
            for j in 0..words.Size()? {
                let word = words.GetAt(j)?;
                row.words.push(word.Text()?.to_string());
                let r = word.BoundingRect()?;
                row.rects.push(RawRect {
                    x: r.X,
                    y: r.Y,
                    w: r.Width,
                    h: r.Height,
                });
            }
            if !row.words.is_empty() {
                lines.push(row);
            }
        }
        Ok(RawOcr { lines, w, h })
    }
}

/// 识别用位图上的原始行（像素坐标，归一化前）。
struct RawOcr {
    lines: Vec<RawLine>,
    w: u32,
    h: u32,
}

struct RawLine {
    words: Vec<String>,
    rects: Vec<RawRect>,
}

struct RawRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
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
            let writer =
                DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0).unwrap()).unwrap();
            writer.WriteBytes(&png).unwrap();
            let flushed = writer.FlushAsync().unwrap().get().unwrap();
            eprintln!(
                "flushed: {flushed}, stream size: {}",
                stream.Size().unwrap()
            );
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
