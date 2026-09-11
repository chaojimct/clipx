//! OCR 拓展包：RapidOCR ONNX（PP-OCRv6 small），cargo feature `rapid` 门控。
//!
//! - 默认构建不含（ort 静态链接约 +25MB 体积）；`clipx-app --features ocr-rapid` 开启。
//! - 模型不进包：`Data/ocr-models/` 存放，首启自动下载（见 clipx-app `ocr_pack`）。
//! - 内存纪律：session 按任务新建、用完即弃（冷加载 ~256ms），常驻零增长。

use std::path::Path;

use crate::OcrWord;

/// 拓展包模型文件名（PPOCRV6_SMALL，稳定名；顺序与 rapidocr-core 注册表一致）。
pub const PACK_FILES: &[&str] = &[
    "PP-OCRv6_det_small.onnx",
    "ch_ppocr_mobile_v2.0_cls_mobile.onnx",
    "PP-OCRv6_rec_small.onnx",
    "ppocrv6_dict.txt",
];

/// 单图行/词上限（与 Media 路径概念对齐）。
pub const LINE_CAP: usize = 200;
pub const WORD_CAP: usize = 1200;

/// 模型是否就绪（4 文件齐且非空；SHA 在下载时校验）。
pub fn models_ready(dir: &Path) -> bool {
    PACK_FILES.iter().all(|f| {
        dir.join(f)
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    })
}

/// 拓展包配置摘要（设置页/托盘展示用；无模型细节，避免 UI 层碰 rapidocr-core）。
pub fn pack_summary() -> &'static str {
    "PP-OCRv6 small（检测+分类+识别，约32MB）"
}

#[cfg(feature = "rapid")]
pub use engine::RapidOcrEngine;
#[cfg(feature = "rapid")]
pub use assets::required_assets;

#[cfg(feature = "rapid")]
mod assets {
    use std::path::PathBuf;

    /// 下载清单（文件名，直链，SHA256）：取 rapidocr-core 注册表，不自建第二份。
    pub fn required_assets() -> Vec<(PathBuf, String, String)> {
        use rapidocr_core::model::PPOCRV6_SMALL;
        PPOCRV6_SMALL
            .assets()
            .into_iter()
            .map(|a| {
                (
                    PathBuf::from(a.filename),
                    a.url.to_string(),
                    a.sha256.unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::super::PACK_FILES;

        #[test]
        fn asset_list_matches_pack_files() {
            let names: Vec<String> = super::required_assets()
                .into_iter()
                .map(|(f, _, _)| f.to_string_lossy().to_string())
                .collect();
            assert_eq!(names, PACK_FILES);
            for (_, url, sha) in super::required_assets() {
                assert!(!url.is_empty());
                assert_eq!(sha.len(), 64, "{url}");
            }
        }
    }
}

#[cfg(feature = "rapid")]
mod engine {
    use std::path::Path;

    use super::{WORD_CAP, LINE_CAP};
    use crate::{OcrDetailed, OcrEngine, OcrLine};

    pub struct RapidOcrEngine {
        ocr: rapidocr_core::RapidOcr,
    }

    impl RapidOcrEngine {
        /// 每次 OCR 任务新建（冷加载 ~256ms），用完即弃：常驻内存零增长。
        pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
            let cfg = rapidocr_core::config::RapidOcrConfig::ppocr_v6_small(model_dir);
            let ocr = rapidocr_core::RapidOcr::from_config(cfg)?;
            Ok(Self { ocr })
        }
    }

    impl OcrEngine for RapidOcrEngine {
        fn recognize_png(&mut self, png: &[u8]) -> anyhow::Result<String> {
            Ok(self.recognize_lines(png)?.text)
        }

        fn recognize_lines(&mut self, png: &[u8]) -> anyhow::Result<OcrDetailed> {
            let img = image::load_from_memory(png)
                .map(|i| i.to_rgb8())
                .map_err(|e| anyhow::anyhow!("拓展包图片解码失败: {e}"))?;
            let (w, h) = (img.width() as f32, img.height() as f32);
            let out = self.ocr.run_image(&img)?;
            let mut text_rows = Vec::new();
            let mut lines = Vec::new();
            let mut word_total = 0;
            for l in &out.lines {
                let t = l.text.trim();
                if t.is_empty() {
                    continue;
                }
                let (x0, y0, x1, y1) = super::quad_bounds(&l.bbox.points, w, h);
                if x1 <= x0 || y1 <= y0 {
                    // 退化框：文本仍保留（可搜可粘），只不做图上叠加
                    text_rows.push(t.to_string());
                    continue;
                }
                text_rows.push(t.to_string());
                let mut words = Vec::new();
                // 行内 token 切词：CJK token 逐字（全角等宽，近似准），
                // 拉丁 token 按词占宽（空格分隔定位）；Media 路径原生词框更准，不动。
                if word_total < WORD_CAP {
                    words = super::split_line_words(t, x0, y0, x1, y1, &mut word_total);
                }
                lines.push(
                    OcrLine {
                        text: t.to_string(),
                        x: x0,
                        y: y0,
                        w: (x1 - x0).max(0.0),
                        h: (y1 - y0).max(0.0),
                        words,
                    }
                    .clamped(),
                );
                if lines.len() >= LINE_CAP {
                    break;
                }
            }
            Ok(OcrDetailed {
                text: text_rows.join("\n"),
                lines,
            })
        }
    }
}

/// 四边形取轴对齐外接框并归一化（rapidocr 输出像素坐标、阅读序）。
/// 无 feature 时仅单测使用。
#[cfg_attr(not(feature = "rapid"), allow(dead_code))]
fn quad_bounds(pts: &[[f32; 2]; 4], w: f32, h: f32) -> (f32, f32, f32, f32) {
    let (mut x0, mut y0, mut x1, mut y1) =
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    if w <= 0.0 || h <= 0.0 || x1 <= x0 || y1 <= y0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (
        (x0 / w).clamp(0.0, 1.0),
        (y0 / h).clamp(0.0, 1.0),
        (x1 / w).clamp(0.0, 1.0),
        (y1 / h).clamp(0.0, 1.0),
    )
}

/// 行内 token 切词（Rapid 行只有行框时）：CJK token 逐字，拉丁 token 按词，
/// 框按字符区间比例映射。调用方维护 word_total 上限。
/// 无 feature 时仅单测使用。
#[cfg_attr(not(feature = "rapid"), allow(dead_code))]
fn split_line_words(
    t: &str,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    word_total: &mut usize,
) -> Vec<OcrWord> {
    let chars: Vec<char> = t.chars().collect();
    let n = chars.len().max(1) as f32;
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        let tx0 = x0 + (x1 - x0) * (start as f32 / n);
        let tx1 = x0 + (x1 - x0) * (i as f32 / n);
        let push = |text: String, ax0: f32, ax1: f32, words: &mut Vec<OcrWord>| {
            words.push(
                OcrWord {
                    text,
                    x: ax0,
                    y: y0,
                    w: (ax1 - ax0).max(0.0),
                    h: (y1 - y0).max(0.0),
                }
                .clamped(),
            );
        };
        if cjk_only(&token) {
            let tc: Vec<char> = token.chars().collect();
            let m = tc.len().max(1) as f32;
            for (k, c) in tc.iter().enumerate() {
                if *word_total >= WORD_CAP {
                    return words;
                }
                *word_total += 1;
                push(
                    c.to_string(),
                    tx0 + (tx1 - tx0) * (k as f32 / m),
                    tx0 + (tx1 - tx0) * ((k + 1) as f32 / m),
                    &mut words,
                );
            }
        } else {
            if *word_total >= WORD_CAP {
                return words;
            }
            *word_total += 1;
            push(token, tx0, tx1, &mut words);
        }
    }
    words
}

/// 纯 CJK 行判定（无 ASCII 字母数字；空格允许）：均分切字的前提。
/// 无 feature 时仅单测使用。
#[cfg_attr(not(feature = "rapid"), allow(dead_code))]
fn cjk_only(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| !c.is_ascii() || c.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_files_ready_check() {
        let dir = std::env::temp_dir().join("clipx-rapid-pack-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!models_ready(&dir));
        std::fs::write(dir.join(PACK_FILES[0]), b"x").unwrap();
        assert!(!models_ready(&dir), "4 件缺 3 件仍未就绪");
        for f in &PACK_FILES[1..] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(models_ready(&dir));
        // 空文件不算就绪
        std::fs::write(dir.join(PACK_FILES[0]), b"").unwrap();
        assert!(!models_ready(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quad_bounds_clamps_and_rejects_degenerate() {
        let (x0, y0, x1, y1) =
            quad_bounds(&[[10.0, 20.0], [110.0, 20.0], [110.0, 40.0], [10.0, 40.0]], 200.0, 100.0);
        assert!((x0 - 0.05).abs() < 1e-6);
        assert!((x1 - 0.55).abs() < 1e-6);
        assert!((y0 - 0.2).abs() < 1e-6);
        assert!((y1 - 0.4).abs() < 1e-6);
        // 越界钳制
        let (x0, _, x1, _) =
            quad_bounds(&[[-5.0, 0.0], [500.0, 0.0], [500.0, 10.0], [-5.0, 10.0]], 200.0, 100.0);
        assert_eq!((x0, x1), (0.0, 1.0));
        // 退化四边形
        assert_eq!(
            quad_bounds(&[[1.0, 1.0], [1.0, 1.0], [1.0, 1.0], [1.0, 1.0]], 200.0, 100.0),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn cjk_only_gate() {
        assert!(cjk_only("你好世界"));
        assert!(cjk_only("你好 世界。！"));
        assert!(!cjk_only("第3页"));
        assert!(!cjk_only("hello"));
        assert!(!cjk_only(""));
    }

    #[test]
    fn split_line_words_mixed() {
        let mut total = 0;
        // "ab 你好 cd"（8 字符归一化框）：拉丁按词，CJK 逐字
        let words = split_line_words("ab 你好 cd", 0.0, 0.0, 1.0, 0.1, &mut total);
        assert_eq!(total, 4);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "你", "好", "cd"]);
        // "ab" 占字符 0..2 → x 0..0.25
        assert!((words[0].x - 0.0).abs() < 1e-6);
        assert!((words[0].w - 0.25).abs() < 1e-6);
        // "你" 占字符 3..4 → x 0.375..0.5
        assert!((words[1].x - 0.375).abs() < 1e-6);
        // "cd" 占字符 6..8 → x 0.75..1.0
        assert!((words[3].x - 0.75).abs() < 1e-6);
        assert!((words[3].w - 0.25).abs() < 1e-6);
        // 上限截断
        let mut total = WORD_CAP;
        let words = split_line_words("你好", 0.0, 0.0, 0.5, 0.1, &mut total);
        assert!(words.is_empty());
    }
}
