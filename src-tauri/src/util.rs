//! 通用工具：时间、大小格式化、类型判定、中文 2-gram。
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else if v < 10.0 {
        format!("{:.2} {}", v, UNITS[i])
    } else if v < 100.0 {
        format!("{:.1} {}", v, UNITS[i])
    } else {
        format!("{:.0} {}", v, UNITS[i])
    }
}

pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic", "avif", "svg",
];
pub const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "3gp"];
pub const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "aiff", "ape"];
pub const DOC_EXTS: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "epub", "mobi", "rtf"];
pub const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "xml", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "html", "htm", "css", "js", "jsx", "ts", "tsx", "vue", "py", "rs", "go",
    "java", "c", "h", "cpp", "hpp", "cs", "php", "rb", "sh", "bat", "ps1", "sql", "lua", "kt",
    "swift", "m", "r", "jl", "tex", "rst", "srt", "ass",
];
pub const ARCHIVE_EXTS: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "cab", "tgz"];
pub const APP_EXTS: &[&str] = &["exe", "msi", "dll", "sys", "apk", "dmg", "pkg", "deb", "rpm", "appx", "msix"];

pub fn ext_kind(ext: &str) -> &'static str {
    let e = ext.to_ascii_lowercase();
    let e = e.as_str();
    if IMAGE_EXTS.contains(&e) {
        "image"
    } else if VIDEO_EXTS.contains(&e) {
        "video"
    } else if AUDIO_EXTS.contains(&e) {
        "audio"
    } else if DOC_EXTS.contains(&e) {
        "document"
    } else if TEXT_EXTS.contains(&e) {
        "text"
    } else if ARCHIVE_EXTS.contains(&e) {
        "archive"
    } else if APP_EXTS.contains(&e) {
        "app"
    } else {
        "other"
    }
}

/// 能抽出正文的 kind。其余类型在 `index.rs::process_one` 里必然被标成
/// `text=skip`，把它们排进全文队列只会白白消耗一次路径检查 + 缩略图。
/// 注意 PDF 属于 `document`，走 `extract_text` 的 pdf 分支。
pub fn kind_has_text(kind: &str) -> bool {
    matches!(kind, "text" | "document")
}

/// 能生成向量（正文或图像特征）的 kind。`archive` / `app` / `other`
/// 既不能抽正文也没有视觉语义，`process_one` 会直接标 `embed=skip`，
/// 因此不进入向量队列 —— 否则它们永远显示为"待处理"，进度条永远不动。
pub fn kind_has_embedding(kind: &str) -> bool {
    matches!(kind, "image" | "audio" | "video" | "text" | "document")
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "image" => "图片",
        "video" => "视频",
        "audio" => "音频",
        "document" => "文档",
        "text" => "文本",
        "archive" => "压缩包",
        "app" => "程序",
        _ => "其他",
    }
}

/// 中文重叠 2-gram + 英文/数字按词切分，用小写。
pub fn cjk_bigrams(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut word = String::new();

    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        if word.len() >= 2 {
            out.push(word.to_lowercase());
        }
        word.clear();
    }
    fn flush_cjk(run: &mut Vec<char>, out: &mut Vec<String>) {
        if run.len() == 1 {
            out.push(run[0].to_string());
        } else {
            for i in 0..run.len().saturating_sub(1) {
                out.push(format!("{}{}", run[i], run[i + 1]));
            }
        }
        run.clear();
    }

    for ch in text.chars() {
        if is_cjk(ch) {
            flush_word(&mut word, &mut out);
            cjk_run.push(ch);
        } else if ch.is_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut out);
            word.push(ch);
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk_run, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk_run, &mut out);
    out
}

pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x3000..=0x303F | 0xFF00..=0xFFEF)
}

/// 入库文本：中文 2-gram 用空格连接，便于 unicode61 分词。
pub fn index_text(text: &str) -> String {
    cjk_bigrams(text).join(" ")
}

/// 查询词：去重保序，返回用于 FTS 的 OR 表达式。
pub fn fts_query_terms(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in cjk_bigrams(query) {
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

pub fn build_fts_expr(query: &str) -> Option<String> {
    let terms = fts_query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let parts: Vec<String> = terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    Some(parts.join(" OR "))
}

pub fn norm_path(p: &str) -> String {
    let s = p.replace('/', "\\");
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '\\' && out.ends_with('\\') {
            continue;
        }
        out.push(ch);
    }
    out
}

pub fn unique_path(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let stem = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let parent = target.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    for i in 1..10_000 {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    target.to_path_buf()
}

pub fn format_ts(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        _ => "-".into(),
    }
}
