//! 文本抽取：纯文本 / RTF / Office(OOXML + 97-2003 OLE2) / PDF / EPUB。
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

pub const MAX_CHARS_DEFAULT: usize = 200_000;
const PDF_IMAGE_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum number of compatible raster images retained for one PDF page.
///
/// A page commonly contains several independent image objects (for example a
/// diagram plus a photograph).  Keeping only the first successfully decoded
/// object made image search miss most of the visual content.  Bound the
/// per-page fan-out so a decorative page with dozens of tiny masks cannot
/// explode the downstream embedding batch or the decoded-image cache.
const PDF_MAX_IMAGES_PER_PAGE: usize = 4;
const PDF_RENDER_FALLBACK_MAX_PAGES: usize = 16;
const PDF_IMAGE_MAX_SIDE: u32 = 512;

fn pdf_image_cache_cost(images: &[(i64, image::DynamicImage)]) -> usize {
    images
        .iter()
        .map(|(_, image)| {
            (image.width() as usize)
                .saturating_mul(image.height() as usize)
                .saturating_mul(4)
        })
        .sum()
}

/// 纯文本类扩展名：直接按字节读 + 编码嗅探。
const PLAIN_EXTS: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "xml", "yaml", "yml", "toml",
    "ini", "cfg", "conf", "html", "htm", "css", "js", "jsx", "ts", "tsx", "vue", "py", "rs", "go",
    "java", "c", "h", "cpp", "hpp", "cs", "php", "rb", "sh", "bat", "ps1", "sql", "lua", "kt",
    "swift", "m", "r", "jl", "tex", "rst", "srt", "ass", "vtt", "nfo", "diff", "patch", "sln",
    "csproj", "gradle", "properties", "env", "gitignore",
];

/// 返回正文。不可抽取的类型返回 None。
pub fn extract_text(path: &Path, max_chars: usize) -> Option<String> {
    extract_text_with_cancel(path, max_chars, || false)
}

/// Extract text while allowing long legacy Office parsing to stop cooperatively.
/// Other formats keep their existing extraction paths and are checked before
/// and after extraction; the OLE Word piece loop also checks between pieces.
pub fn extract_text_with_cancel<F>(path: &Path, max_chars: usize, cancelled: F) -> Option<String>
where
    F: Fn() -> bool,
{
    if cancelled() {
        return None;
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if PLAIN_EXTS.contains(&ext.as_str()) {
        let result = Some(read_plain(path, max_chars));
        return if cancelled() { None } else { result };
    }
    // Keep page/slide boundaries in the raw body. The database stores the
    // aggregate in the existing file-level FTS row and mirrors each form-feed
    // separated part into its page index for precise hit locations.
    if matches!(ext.as_str(), "pdf" | "docx" | "pptx") {
        let result = extract_pages(path, max_chars).map(|pages| {
            truncate_chars(&pages.join("\u{c}"), max_chars)
        });
        return if cancelled() { None } else { result };
    }
    let text = match ext.as_str() {
        "docx" => ooxml_text(path, "word/document.xml", "w:t", max_chars),
        "pptx" => ooxml_slides(path, max_chars),
        "xlsx" => ooxml_xlsx(path, max_chars),
        "doc" => ole_doc_text_with_cancel(path, max_chars, &cancelled),
        "xls" => ole_xls_text(path, max_chars),
        "ppt" => ole_ppt_text(path, max_chars),
        "odt" | "ods" | "odp" | "odg" => opendocument_text(path, max_chars),
        "pdf" => pdf_text(path, max_chars),
        "epub" => epub_text(path, max_chars),
        "rtf" => rtf_text(path, max_chars),
        _ => return None,
    };
    if cancelled() { None } else { Some(text) }
}

/// Extract logical pages for formats where the container exposes a reliable
/// page/slide boundary. DOCX uses explicit page breaks when present; files
/// without them remain a single logical page and are chunked by the database.
pub fn extract_pages(path: &Path, max_chars: usize) -> Option<Vec<String>> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => {
            if !pdf_has_text_operator(path) {
                return Some(Vec::new());
            }
            let pages = pdf_pages_with_encoding_fallback(path, max_chars)?;
            if pages.is_empty() {
                let fallback = pdf_text(path, max_chars);
                return Some(if fallback.trim().is_empty() { Vec::new() } else { vec![fallback] });
            }
            let mut used = 0usize;
            Some(
                pages
                    .into_iter()
                    .map(|page| {
                        let remaining = max_chars.saturating_sub(used);
                        let value = truncate_chars(&page, remaining);
                        used = used.saturating_add(value.chars().count());
                        value
                    })
                    .collect(),
            )
        }
        "docx" => Some(ooxml_docx_pages(path, max_chars)),
        "pptx" => Some(ooxml_slide_pages(path, max_chars)),
        _ => extract_text(path, max_chars).map(|text| vec![text]),
    }
}

/// Return the physical page count even when a scanned PDF has no text
/// operators. This lets the OCR stage preserve blank pages and their numbers.
pub fn pdf_page_count(path: &Path) -> Option<usize> {
    let document = lopdf::Document::load(path).ok()?;
    Some(document.get_pages().len())
}

/// List one-based PDF pages that contain embedded image objects. This is a
/// lightweight structural check used to route OCR only to image-backed pages
/// with little extractable text; it does not decode image pixels.
pub fn pdf_pages_with_images_with_cancel<F>(path: &Path, cancelled: F) -> Vec<i64>
where
    F: Fn() -> bool,
{
    let Ok(document) = lopdf::Document::load(path) else {
        return Vec::new();
    };
    document
        .page_iter()
        .enumerate()
        .filter_map(|(index, page_id)| {
            if cancelled() {
                return None;
            }
            document
                .get_page_images(page_id)
                .is_ok_and(|images| !images.is_empty())
                .then_some(index as i64 + 1)
        })
        .collect()
}

fn read_plain(path: &Path, max_chars: usize) -> String {
    match std::fs::read(path) {
        Ok(b) => truncate_chars(&decode_bytes(&b), max_chars),
        Err(_) => String::new(),
    }
}

/// 依次尝试 BOM / UTF-8 / GB18030 / 有损 UTF-8。
pub fn decode_bytes(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).to_string();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode(rest).0.to_string();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return encoding_rs::UTF_16BE.decode(rest).0.to_string();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (s, _, had_errors) = encoding_rs::GB18030.decode(bytes);
    if !had_errors || s.chars().filter(|c| *c == '\u{FFFD}').count() < s.len() / 20 {
        s.to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// Append at most `max_chars` characters while keeping a running count. Legacy
/// Office streams may contain many tiny text records, so rescanning the full
/// output with `out.chars().count()` for each record is quadratic.
fn append_limited_chars(out: &mut String, text: &str, total: &mut usize, max_chars: usize) {
    let remaining = max_chars.saturating_sub(*total);
    if remaining == 0 {
        return;
    }
    let mut end = 0usize;
    let mut appended = 0usize;
    for (index, ch) in text.char_indices().take(remaining) {
        end = index + ch.len_utf8();
        appended += 1;
    }
    out.push_str(&text[..end]);
    *total = (*total).saturating_add(appended);
}

fn keep_printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_nl = false;
    for ch in s.chars() {
        // Word 97 用 \r 分段、\x07 表示单元格结束，直接丢掉会让整篇正文粘成一串
        if ch == '\r' || ch == '\u{7}' || ch == '\u{b}' {
            if !last_nl {
                out.push('\n');
                last_nl = true;
            }
            continue;
        }
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
            last_nl = ch == '\n';
        }
    }
    out
}

// ---------------------------------------------------------------- OOXML / zip
fn open_zip(path: &Path) -> Option<zip::ZipArchive<std::fs::File>> {
    zip::ZipArchive::new(std::fs::File::open(path).ok()?).ok()
}

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    max: usize,
) -> Option<String> {
    let f = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    f.take((max * 4) as u64).read_to_end(&mut buf).ok()?;
    Some(decode_bytes(&buf))
}

/// 抽出指定标签内的文本。
fn xml_tag_text(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i..];
        let Some(gt) = after.find('>') else { break };
        let body = &after[gt + 1..];
        let Some(end) = body.find(&close) else { break };
        out.push_str(&xml_unescape(&body[..end]));
        rest = &body[end + close.len()..];
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn ooxml_text(path: &Path, entry: &str, tag: &str, max: usize) -> String {
    let Some(mut zip) = open_zip(path) else { return String::new() };
    let Some(xml) = read_zip_entry(&mut zip, entry, max) else { return String::new() };
    // DOCX has no fixed page geometry in the package. Preserve explicit page
    // breaks when present; Db::set_doc will use bounded chunks for flowing
    // documents that have no break markers.
    let xml = xml
        .replace("<w:br w:type=\"page\"/>", "\u{c}")
        .replace("<w:br w:type=\"page\" />", "\u{c}");
    truncate_chars(&xml_paragraph_text(&xml, tag, "</w:p>"), max)
}

fn ooxml_docx_pages(path: &Path, max: usize) -> Vec<String> {
    let Some(mut zip) = open_zip(path) else { return Vec::new() };
    let Some(xml) = read_zip_entry(&mut zip, "word/document.xml", max.saturating_mul(2).max(max)) else {
        return Vec::new();
    };
    let mut pages = Vec::new();
    let mut rest = xml.as_str();
    let mut used = 0usize;
    loop {
        let split_at = rest
            .find("w:type=\"page\"")
            .or_else(|| rest.find("w:lastRenderedPageBreak"));
        let (part, tail) = match split_at {
            Some(offset) => {
                let end = rest[offset..].find('>').map(|v| offset + v + 1).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
            None => (rest, ""),
        };
        let value = xml_paragraph_text(part, "w:t", "</w:p>");
        let remaining = max.saturating_sub(used);
        if remaining == 0 { break; }
        let value = truncate_chars(&value, remaining);
        used = used.saturating_add(value.chars().count());
        // Keep empty slots so a later Word page retains its original number.
        pages.push(value);
        if tail.is_empty() { break; }
        rest = tail;
    }
    pages
}

fn ooxml_slide_pages(path: &Path, max: usize) -> Vec<String> {
    let Some(mut zip) = open_zip(path) else { return Vec::new() };
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        .map(str::to_owned)
        .collect();
    names.sort_by_key(|name| {
        name.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    let mut pages = Vec::new();
    let mut used = 0usize;
    for name in names {
        let Some(xml) = read_zip_entry(&mut zip, &name, max.saturating_sub(used).max(1)) else { continue };
        let remaining = max.saturating_sub(used);
        if remaining == 0 { break; }
        let value = truncate_chars(&xml_tag_text(&xml, "a:t"), remaining);
        used = used.saturating_add(value.chars().count());
        // Keep empty slots so empty slides do not renumber later slides.
        pages.push(value);
    }
    pages
}

/// 按段落抽文本：整篇 `xml_tag_text` 会把所有段落粘成一长串，
/// 「项目周报 第42周本周完成…」这种连读在片段预览里很难看，也不利于
/// 按段落做命中定位。按段尾标签切开，段内抽 tag，段间补换行。
fn xml_paragraph_text(xml: &str, tag: &str, para_end: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(i) = rest.find(para_end) {
        let (line, tail) = rest.split_at(i + para_end.len());
        let t = xml_tag_text(line, tag);
        if !t.trim().is_empty() {
            out.push_str(&t);
            out.push('\n');
        }
        rest = tail;
    }
    // 最后一小段（没有段落结束标签的残缺 xml）
    let t = xml_tag_text(rest, tag);
    if !t.trim().is_empty() {
        out.push_str(&t);
        out.push('\n');
    }
    out
}

/// 遍历 zip 内匹配前缀的 xml 条目，抽取 tag 文本（pptx 幻灯片 / odt 等复用）。
fn zip_entries_text<F: Fn(&str) -> bool>(
    path: &Path,
    match_fn: F,
    tag: &str,
    max: usize,
) -> String {
    let Some(mut zip) = open_zip(path) else { return String::new() };
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| match_fn(n))
        .map(|s| s.to_string())
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        if let Some(xml) = read_zip_entry(&mut zip, &n, max) {
            if !out.is_empty() {
                out.push('\u{c}');
            }
            out.push_str(&xml_tag_text(&xml, tag));
            out.push('\n');
        }
    }
    truncate_chars(&out, max)
}

fn ooxml_slides(path: &Path, max: usize) -> String {
    zip_entries_text(
        path,
        |n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"),
        "a:t",
        max,
    )
}

fn ooxml_xlsx(path: &Path, max: usize) -> String {
    let Some(mut zip) = open_zip(path) else { return String::new() };
    let mut shared: Vec<String> = Vec::new();
    if let Some(xml) = read_zip_entry(&mut zip, "xl/sharedStrings.xml", max) {
        let mut rest = xml.as_str();
        while let Some(i) = rest.find("<si>") {
            let body = &rest[i + 4..];
            let Some(end) = body.find("</si>") else { break };
            shared.push(xml_tag_text(&body[..end], "t"));
            rest = &body[end + 5..];
        }
    }
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        if let Some(xml) = read_zip_entry(&mut zip, &n, max) {
            let mut rest = xml.as_str();
            while let Some(i) = rest.find("<c") {
                let body = &rest[i..];
                let Some(end) = body.find("</c>") else { break };
                let cell = &body[..end];
                if cell.contains("t=\"s\"") {
                    if let Some(idx) = cell
                        .find("<v>")
                        .and_then(|p| cell[p + 3..].find("</v>").map(|e| &cell[p + 3..p + 3 + e]))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                    {
                        if let Some(s) = shared.get(idx) {
                            out.push_str(s);
                            out.push('\t');
                        }
                    }
                } else if cell.contains("t=\"inlineStr\"") || cell.contains("<is>") {
                    out.push_str(&xml_tag_text(cell, "t"));
                    out.push('\t');
                } else {
                    out.push_str(&xml_tag_text(cell, "v"));
                    out.push('\t');
                }
                rest = &body[end + 4..];
            }
            out.push('\n');
        }
    }
    truncate_chars(&out, max)
}

fn opendocument_text(path: &Path, max: usize) -> String {
    let Some(mut zip) = open_zip(path) else { return String::new() };
    let Some(xml) = read_zip_entry(&mut zip, "content.xml", max) else { return String::new() };
    // 段落/单元格/标题之间补分隔，否则所有文字粘成一坨
    let spaced = xml
        .replace("</text:p>", "\n")
        .replace("</text:h>", "\n")
        .replace("</table:table-cell>", "\t");
    truncate_chars(&strip_html(&spaced), max)
}

fn pdf_pages_with_encoding_fallback(path: &Path, max_chars: usize) -> Option<Vec<String>> {
    // Keep the normal layout-aware extractor as the only pass for supported
    // PDFs. Some older Chinese PDFs use a named GBK CMap that it rejects.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_by_pages(path).ok()
    }))
    .ok()
    .flatten()
    .or_else(|| pdf_gbk_pages_fallback(path, max_chars))
}

fn pdf_font_uses_gbk_source(doc: &lopdf::Document, font: &lopdf::Dictionary) -> bool {
    // GBK-EUC-H explicitly defines the source bytes as GBK. Do not infer
    // Unicode from arbitrary CID/Identity encodings or override ToUnicode.
    font.get_deref(b"Encoding", doc).and_then(lopdf::Object::as_name).ok() == Some(b"GBK-EUC-H")
        && font.get(b"ToUnicode").is_err()
}

enum PdfFallbackEncoding<'a> {
    Gbk,
    Font(lopdf::Encoding<'a>),
}

fn append_pdf_fallback_operands(
    operands: &[lopdf::Object],
    encoding: &PdfFallbackEncoding<'_>,
    text: &mut String,
    used: &mut usize,
    max_chars: usize,
) {
    for operand in operands {
        if *used >= max_chars {
            break;
        }
        match operand {
            lopdf::Object::String(bytes, _) => {
                let decoded = match encoding {
                    PdfFallbackEncoding::Gbk => encoding_rs::GBK
                        .decode_without_bom_handling_and_without_replacement(bytes)
                        .map(|text| text.into_owned()),
                    PdfFallbackEncoding::Font(encoding) => encoding.bytes_to_string(bytes).ok(),
                };
                if let Some(decoded) = decoded {
                    append_limited_chars(text, &decoded, used, max_chars);
                }
            }
            lopdf::Object::Array(items) => {
                append_pdf_fallback_operands(items, encoding, text, used, max_chars);
            }
            lopdf::Object::Integer(value) if *value < -100 => {
                append_limited_chars(text, " ", used, max_chars);
            }
            _ => {}
        }
    }
}

fn pdf_gbk_page_text(doc: &lopdf::Document, page_id: lopdf::ObjectId, max_chars: usize) -> Option<String> {
    let fonts = doc.get_page_fonts(page_id).ok()?;
    let encodings: std::collections::HashMap<_, _> = fonts.into_iter().filter_map(|(name, font)| {
        let encoding = if pdf_font_uses_gbk_source(doc, font) {
            PdfFallbackEncoding::Gbk
        } else {
            PdfFallbackEncoding::Font(font.get_font_encoding(doc).ok()?)
        };
        Some((name, encoding))
    }).collect();
    let content = doc.get_and_decode_page_content(page_id).ok()?;
    let mut text = String::new();
    let mut used = 0;
    let mut current_font = None;
    let mut saved_fonts = Vec::new();
    for operation in content.operations {
        if used >= max_chars {
            break;
        }
        match operation.operator.as_str() {
            "Tf" => {
                current_font = operation.operands.first().and_then(|name| name.as_name().ok()).map(|name| name.to_vec());
            }
            "q" => saved_fonts.push(current_font.clone()),
            "Q" => if let Some(font) = saved_fonts.pop() { current_font = font; },
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(operation.operator.as_str(), "'" | "\"") && !text.is_empty() && !text.ends_with('\n') {
                    append_limited_chars(&mut text, "\n", &mut used, max_chars);
                }
                if let Some(encoding) = current_font.as_ref().and_then(|name| encodings.get(name)) {
                    let operands = if operation.operator == "\"" {
                        operation.operands.get(2..).unwrap_or_default()
                    } else {
                        operation.operands.as_slice()
                    };
                    append_pdf_fallback_operands(operands, encoding, &mut text, &mut used, max_chars);
                }
            }
            "ET" | "T*" => if !text.is_empty() && !text.ends_with('\n') {
                append_limited_chars(&mut text, "\n", &mut used, max_chars);
            },
            "Td" | "TD" if operation.operands.get(1).and_then(|value| value.as_float().ok()).is_some_and(|dy| dy != 0.0) => {
                if !text.is_empty() && !text.ends_with('\n') {
                    append_limited_chars(&mut text, "\n", &mut used, max_chars);
                }
            }
            _ => {}
        }
    }
    Some(text)
}

/// A narrowly scoped rescue pass for legacy GBK-EUC-H documents. It extracts
/// source text without inventing CID mappings or glyph widths and retains one
/// slot per physical page, including blank/unsupported pages.
fn pdf_gbk_pages_fallback(path: &Path, max_chars: usize) -> Option<Vec<String>> {
    let doc = lopdf::Document::load(path).ok()?;
    if !doc.objects.values().filter_map(|object| object.as_dict().ok()).any(|font| pdf_font_uses_gbk_source(&doc, font)) {
        return None;
    }
    let mut used = 0;
    Some(doc.get_pages().into_iter().map(|(page, page_id)| {
        let remaining = max_chars.saturating_sub(used);
        let text = if remaining == 0 {
            String::new()
        } else {
            doc.extract_text(&[page]).ok()
                .or_else(|| pdf_gbk_page_text(&doc, page_id, remaining))
                .unwrap_or_default()
        };
        let text = truncate_chars(&text, remaining);
        used = used.saturating_add(text.chars().count());
        text
    }).collect())
}

fn pdf_text(path: &Path, max: usize) -> String {
    // Scanned PDFs often contain only image draw operations. Avoid sending
    // those files through pdf-extract: it walks every embedded font and can
    // emit thousands of harmless "unknown glyph name" lines for Dingbats and
    // damaged font maps. A page content stream with a text operator is enough
    // to justify the more expensive extractor.
    if !pdf_has_text_operator(path) {
        return String::new();
    }
    // pdf-extract has a few malformed-font assertions. A damaged PDF should
    // become an empty extraction result, never unwind the index worker.
    pdf_pages_with_encoding_fallback(path, max)
        .unwrap_or_default()
        .into_iter()
        .map(|page| truncate_chars(&page, max))
        .filter(|page| !page.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\u{c}")
}

fn pdf_has_text_operator(path: &Path) -> bool {
    let Ok(doc) = lopdf::Document::load(path) else {
        // Keep the previous best-effort behavior when lopdf cannot parse a
        // damaged file; pdf-extract may still recover useful text.
        return true;
    };
    pdf_document_may_have_text(&doc)
}

/// This is only a fast negative filter for image-only PDFs. Incomplete or
/// unfamiliar content must reach the full extractor instead of being silently
/// classified as a scan. PDF delimiters end operands without whitespace, so
/// splitting bytes on spaces misses perfectly valid `(text)Tj` / `[(text)]TJ`.
fn pdf_content_may_have_text(stream: &lopdf::Stream) -> bool {
    let Ok(content) = stream.get_plain_content() else {
        return true;
    };
    // lopdf's Flate decoder can swallow a corrupt-stream error and return an
    // empty buffer. That is not evidence that the source contained no text.
    if content.is_empty() && !stream.content.is_empty() && stream.dict.get(b"Filter").is_ok() {
        return true;
    }
    let Ok(content) = lopdf::content::Content::decode(&content) else {
        return true;
    };
    content.operations.iter().any(|operation| {
        matches!(operation.operator.as_str(), "Tj" | "TJ" | "'" | "\"")
    })
}

fn pdf_document_may_have_text(doc: &lopdf::Document) -> bool {
    for page_id in doc.page_iter() {
        for content_id in doc.get_page_contents(page_id) {
            let Ok(stream) = doc.get_object(content_id).and_then(lopdf::Object::as_stream) else {
                return true;
            };
            if pdf_content_may_have_text(stream) {
                return true;
            }
        }
    }
    // A page can contain only `/Fm0 Do`, while all its text is inside a Form
    // XObject (possibly nested/compressed or using inherited resources). Scan
    // Form streams once, without walking resource trees or recursive `Do`
    // calls. Including an unused Form is a safe false positive for this gate.
    doc.objects.values().any(|object| {
        let Ok(stream) = object.as_stream() else {
            return false;
        };
        stream.dict.get(b"Subtype").and_then(lopdf::Object::as_name).ok() == Some(b"Form")
            && pdf_content_may_have_text(stream)
    })
}

/// Decode common embedded PDF raster images for multimodal indexing. JPEG
/// streams are passed through directly; simple 8-bit RGB/gray Flate streams
/// are reconstructed from their declared dimensions. Unsupported PDF image
/// encodings are skipped without failing the document.
pub fn pdf_images(path: &Path, limit: usize) -> Vec<image::DynamicImage> {
    pdf_images_with_cancel(path, limit, || false)
}

/// Same decoder with a cooperative cancellation hook. PDF decompression itself
/// is library code and cannot be interrupted, but checking between pages and
/// embedded images keeps a large scanned document from delaying cancellation.
pub fn pdf_images_with_cancel<F>(path: &Path, limit: usize, cancelled: F) -> Vec<image::DynamicImage>
where
    F: Fn() -> bool,
{
    pdf_images_by_page_with_cancel(path, limit, cancelled)
        .into_iter()
        .map(|(_, image)| image)
        .collect()
}

fn evenly_sampled_indices(length: usize, limit: usize) -> Vec<usize> {
    let count = limit.min(length);
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![0];
    }
    (0..count)
        .map(|index| index * (length - 1) / (count - 1))
        .collect()
}

/// Image objects with the same dimensions on three consecutive PDF pages are
/// commonly repeated watermarks, logos, or UI icons. Return the dimensions to
/// exclude while keeping isolated or non-consecutive occurrences searchable.
fn repeated_pdf_image_dimensions(
    page_dimensions: &[Vec<(i64, i64)>],
) -> std::collections::HashSet<(usize, (i64, i64))> {
    let mut pages_by_size: std::collections::HashMap<(i64, i64), Vec<usize>> =
        std::collections::HashMap::new();
    for (page_index, dimensions) in page_dimensions.iter().enumerate() {
        for size in dimensions.iter().copied().collect::<std::collections::HashSet<_>>() {
            pages_by_size.entry(size).or_default().push(page_index);
        }
    }

    let mut repeated = std::collections::HashSet::new();
    for (size, mut indices) in pages_by_size {
        indices.sort_unstable();
        let mut run_start = 0;
        while run_start < indices.len() {
            let mut run_end = run_start + 1;
            while run_end < indices.len() && indices[run_end] == indices[run_end - 1] + 1 {
                run_end += 1;
            }
            if run_end - run_start >= 3 {
                repeated.extend(indices[run_start..run_end].iter().map(|&page| (page, size)));
            }
            run_start = run_end;
        }
    }
    repeated
}

fn pdf_render_fallback_candidates(
    page_dimensions: &[Vec<(i64, i64)>],
    repeated_dimensions: &std::collections::HashSet<(usize, (i64, i64))>,
    selected_image_pages: &std::collections::HashSet<usize>,
) -> Vec<usize> {
    page_dimensions
        .iter()
        .enumerate()
        .filter_map(|(page_index, dimensions)| {
            // Unselected image pages must not be rendered as full-page images:
            // that would bypass the image-page sampling cap. Likewise, don't
            // reintroduce a filtered watermark by rendering the page composite.
            let has_repeated_image = dimensions.iter().any(|size| {
                repeated_dimensions.contains(&(page_index, *size))
            });
            (dimensions.is_empty()
                || (selected_image_pages.contains(&page_index) && !has_repeated_image))
                .then_some(page_index)
        })
        .collect()
}

/// Decode common embedded PDF raster images and retain their one-based page
/// number so semantic vectors can point to the page that contains the image.
pub fn pdf_images_by_page_with_cancel<F>(
    path: &Path,
    limit: usize,
    cancelled: F,
) -> Vec<(i64, image::DynamicImage)>
where
    F: Fn() -> bool,
{
    // PDF image extraction is often requested twice for the same file: once
    // for the primary page vectors and again by an optional OCR/cross-modal
    // enrichment pass. Keep a small bounded cache keyed by the file's
    // identity and requested page limit so the second pass reuses decoded
    // rasters instead of reopening and inflating every PDF object.
    type PdfImageCache = std::collections::HashMap<(std::path::PathBuf, u64, u64, usize), Vec<(i64, image::DynamicImage)>>;
    static CACHE: OnceLock<Mutex<PdfImageCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(PdfImageCache::new()));
    let (modified, size) = std::fs::metadata(path)
        .ok()
        .map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_nanos() as u64)
                .unwrap_or_default();
            (modified, meta.len())
        })
        .unwrap_or_default();
    let key = (path.to_path_buf(), modified, size, limit);
    if let Ok(guard) = cache.lock() {
        if let Some(images) = guard.get(&key) {
            return images.clone();
        }
    }
    let images = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(doc) = lopdf::Document::load(path) else { return Vec::new() };
        let pages: Vec<_> = doc.page_iter().collect();
        if pages.is_empty() || limit == 0 {
            return Vec::new();
        }
        let page_count = pages.len();
        // Sampling physical pages first can miss sparse image pages entirely
        // (for example, a 500-page manual with illustrations every 40 pages).
        // Find image-bearing pages cheaply, then sample that smaller set.
        let mut page_dimensions = Vec::with_capacity(page_count);
        for page_id in &pages {
            if cancelled() {
                return Vec::new();
            }
            let images = doc.get_page_images(*page_id).unwrap_or_default();
            page_dimensions.push(
                images
                    .iter()
                    .map(|image| (image.width, image.height))
                    .collect::<Vec<_>>(),
            );
        }
        let repeated_dimensions = repeated_pdf_image_dimensions(&page_dimensions);
        let image_pages: Vec<usize> = page_dimensions
            .iter()
            .enumerate()
            .filter_map(|(index, dimensions)| {
                dimensions
                    .iter()
                    .any(|size| !repeated_dimensions.contains(&(index, *size)))
                    .then_some(index)
            })
            .collect();
        let selected_image_pages: std::collections::HashSet<usize> =
            evenly_sampled_indices(image_pages.len(), limit)
                .into_iter()
                .map(|selected| image_pages[selected])
                .collect();
        let mut out = Vec::new();
        let mut decoded_page_indices: std::collections::HashSet<usize> = page_dimensions
            .iter()
            .enumerate()
            .filter_map(|(page_index, dimensions)| {
                (!dimensions.is_empty()
                    && dimensions.iter().all(|size| {
                        repeated_dimensions.contains(&(page_index, *size))
                    }))
                .then_some(page_index)
            })
            .collect();
        // Keep every page occurrence so page search and previews retain the
        // correct page number. The semantic writer deduplicates identical
        // rasters before model inference, so repeated logos are still only
        // encoded once.
        for (page_index, page_id) in pages.into_iter().enumerate() {
            if cancelled() { return out; }
            if !selected_image_pages.contains(&page_index) { continue; }
            let page_no = page_index as i64 + 1;
            let Ok(images) = doc.get_page_images(page_id) else { continue };
            if images.is_empty() { continue; }
            let has_repeated_dimensions = images.iter().any(|image| {
                repeated_dimensions.contains(&(page_index, (image.width, image.height)))
            });
            if has_repeated_dimensions {
                // Do not let the full-page render fallback reintroduce a
                // filtered repeated watermark as part of a composite page.
                decoded_page_indices.insert(page_index);
            }
            let mut images = images;
            images.sort_by_key(|item| std::cmp::Reverse(item.width.saturating_mul(item.height)));
            let mut page_image_count = 0usize;
            for item in images {
                if cancelled() { return out; }
                if repeated_dimensions.contains(&(page_index, (item.width, item.height))) {
                    continue;
                }
                if page_image_count >= PDF_MAX_IMAGES_PER_PAGE {
                    break;
                }
                let filters = item.filters.clone().unwrap_or_default();
                let bytes = if filters.iter().any(|f| f.eq_ignore_ascii_case("DCTDecode")) {
                    item.content.to_vec()
                } else {
                    let mut dict = item.origin_dict.clone();
                    dict.remove(b"Subtype");
                    let stream = lopdf::Stream::new(dict, item.content.to_vec());
                    match stream.decompressed_content() { Ok(v) => v, Err(_) => continue }
                };
                if filters.iter().any(|f| f.eq_ignore_ascii_case("DCTDecode")) {
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        let img = img.thumbnail(PDF_IMAGE_MAX_SIDE, PDF_IMAGE_MAX_SIDE).to_rgb8();
                        out.push((page_no, image::DynamicImage::ImageRgb8(img)));
                        page_image_count += 1;
                        decoded_page_indices.insert(page_index);
                    }
                    continue;
                }
                if item.width <= 0 || item.height <= 0 || item.width > 20_000 || item.height > 20_000
                    || item.width.saturating_mul(item.height) > 100_000_000 { continue; }
                let width = item.width.max(1) as u32;
                let height = item.height.max(1) as u32;
                let color = item.color_space.as_deref().unwrap_or("").to_ascii_lowercase();
                let image = if color.contains("gray") && bytes.len() >= (width as usize * height as usize) {
                    image::GrayImage::from_raw(width, height, bytes[..width as usize * height as usize].to_vec())
                        .map(image::DynamicImage::ImageLuma8)
                } else if color.contains("rgb") && bytes.len() >= (width as usize * height as usize * 3) {
                    image::RgbImage::from_raw(width, height, bytes[..width as usize * height as usize * 3].to_vec())
                        .map(image::DynamicImage::ImageRgb8)
                } else { None };
                if let Some(img) = image {
                    let img = img.thumbnail(PDF_IMAGE_MAX_SIDE, PDF_IMAGE_MAX_SIDE).to_rgb8();
                    out.push((page_no, image::DynamicImage::ImageRgb8(img)));
                    page_image_count += 1;
                    decoded_page_indices.insert(page_index);
                }
            }
        }

        // lopdf exposes embedded raster objects, not the final composited PDF
        // page. Vector diagrams, masks, and unsupported color spaces therefore
        // need a renderer fallback. Sample only a bounded number of pages that
        // still have no decodable raster so large manuals do not trigger a
        // full-document raster pass.
        let fallback_candidates = pdf_render_fallback_candidates(
            &page_dimensions,
            &repeated_dimensions,
            &selected_image_pages,
        )
        .into_iter()
        .filter(|index| !decoded_page_indices.contains(index))
        .collect::<Vec<_>>();
        let fallback_count = PDF_RENDER_FALLBACK_MAX_PAGES.min(limit);
        for index in evenly_sampled_indices(fallback_candidates.len(), fallback_count) {
            if cancelled() {
                return out;
            }
            let page_index = fallback_candidates[index];
            if let Ok(image) = crate::ocr::render_pdf_page_image(path, page_index + 1, &cancelled) {
                let image = image.thumbnail(PDF_IMAGE_MAX_SIDE, PDF_IMAGE_MAX_SIDE).to_rgb8();
                out.push((page_index as i64 + 1, image::DynamicImage::ImageRgb8(image)));
            }
        }
        out
    }))
    .unwrap_or_default();
    let image_cost = pdf_image_cache_cost(&images);
    if !images.is_empty() && image_cost <= PDF_IMAGE_CACHE_BYTES {
        if let Ok(mut guard) = cache.lock() {
            // Bound retained decoded raster memory rather than only the entry
            // count: a single PDF can contain many high-resolution pages.
            // This cache is shared with non-indexing callers; index.rs has its
            // own larger bounded cache for the active multi-file pipeline.
            guard.remove(&key);
            while guard.len() >= 32
                || guard
                    .values()
                    .map(|cached| pdf_image_cache_cost(cached))
                    .fold(0usize, usize::saturating_add)
                    .saturating_add(image_cost)
                    > PDF_IMAGE_CACHE_BYTES
            {
                let Some(evict) = guard.keys().next().cloned() else { break };
                guard.remove(&evict);
            }
            guard.insert(key, images.clone());
        }
    }
    images
}

fn epub_text(path: &Path, max: usize) -> String {
    let Some(mut zip) = open_zip(path) else { return String::new() };
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm"))
        .map(|s| s.to_string())
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        if let Some(html) = read_zip_entry(&mut zip, &n, max) {
            out.push_str(&strip_html(&html));
            out.push('\n');
        }
    }
    truncate_chars(&out, max)
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() / 2);
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    xml_unescape(&out)
}

// ---------------------------------------------------------------- RTF
/// 目标组：这些控制词后面紧跟的 `{...}` 是元数据（字体表、样式表、图片、
/// 文档信息），不是正文，整组跳过。
const SKIP_GROUPS: &[&str] = &[
    "fonttbl", "colortbl", "stylesheet", "info", "pict", "object", "header", "footer",
    "themedata", "colorschememapping", "latentstyles", "datastore", "listtable", "listoverridetable",
];

/// 极简 RTF：丢弃控制字，处理 \uN / \'hh 转义。
///
/// 字体表 / 样式表 / 图片等「目标组」整组跳过：不跳的话，字体名
/// （`Helvetica;`）会被当成正文塞进索引，搜 "helvet" 反而命中一堆文档。
fn rtf_text(path: &Path, max: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    let src = String::from_utf8_lossy(&bytes);
    let mut out = String::new();
    let mut chars = src.chars().peekable();
    let mut pending_hi: Option<u8> = None;
    let mut depth: usize = 0;
    let mut skip_to: Option<usize> = None; // 跳到该深度的 `}` 为止
    while let Some(c) = chars.next() {
        if let Some(d) = skip_to {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth < d {
                        skip_to = None;
                    }
                }
                '\\' => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        match c {
            '\\' => match chars.next() {
                Some('u') => {
                    let mut num = String::new();
                    if chars.peek() == Some(&'-') {
                        num.push('-');
                        chars.next();
                    }
                    while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                        num.push(*d);
                        chars.next();
                    }
                    if let Ok(v) = num.parse::<i32>() {
                        if let Some(ch) = char::from_u32(if v < 0 { v + 65536 } else { v } as u32) {
                            out.push(ch);
                        }
                    }
                    // 跳过 \ucN 指定的占位字符（通常 1 个）
                    chars.next();
                }
                Some('\'') => {
                    let hi = chars.next().and_then(|a| chars.next().and_then(|b| {
                        u8::from_str_radix(&format!("{a}{b}"), 16).ok()
                    }));
                    if let Some(b) = hi {
                        // 两字节 GBK 组合
                        match (pending_hi, b) {
                            (Some(h), _) if h >= 0x81 => {
                                let pair = [h, b];
                                out.push_str(&decode_bytes(&pair));
                                pending_hi = None;
                            }
                            _ if b >= 0x81 => pending_hi = Some(b),
                            _ => out.push(b as char),
                        }
                    }
                }
                Some(other) => {
                    // 控制词：字母序列 + 可选数字 + 可选空格
                    let mut word = String::new();
                    word.push(other);
                    let mut is_alpha = other.is_ascii_alphabetic();
                    while let Some(&n) = chars.peek() {
                        if n.is_ascii_alphabetic() && is_alpha {
                            word.push(n);
                            chars.next();
                        } else if n.is_ascii_digit() || n == '-' {
                            is_alpha = false;
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if chars.peek() == Some(&' ') {
                        chars.next();
                    }
                    let w = word.as_str();
                    if SKIP_GROUPS.iter().any(|g| w == *g) {
                        // 接下来这一组是字体表/样式表/图片，整组丢弃
                        while let Some(&n) = chars.peek() {
                            if n == '{' {
                                depth += 1;
                                chars.next();
                                break;
                            }
                            chars.next();
                        }
                        skip_to = Some(depth);
                        continue;
                    }
                    if w.starts_with("par") || w.starts_with("line") || w.starts_with("row") {
                        out.push('\n');
                    } else if w.starts_with("tab") || w.starts_with("cell") {
                        out.push('\t');
                    }
                }
                None => break,
            },
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '\r' | '\n' => {}
            c => out.push(c),
        }
        if out.chars().count() >= max {
            break;
        }
    }
    truncate_chars(&keep_printable(&out), max)
}

// ---------------------------------------------------------------- OLE2 (97-2003)
//
// Word/Excel/PowerPoint 97-2003 都是 OLE2 复合文档（CFB）。
// 只用「读根目录 → 按名字找流 → 读 MiniFAT/FAT 链」这一小撮能力，
// 自己实现约 120 行，比引入 cfb + fnv + uuid 三个 crate 更划算。

struct Ole {
    data: Vec<u8>,
    sector_size: usize,
    mini_size: usize,
    mini_cutoff: usize,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    mini_stream: Vec<u8>,
    entries: Vec<(String, u8, u32, u64)>, // 名称 / 类型 / 起始 / 长度
}

fn ole_open(path: &Path) -> Option<Ole> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 512 || &data[0..8] != b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1" {
        return None;
    }
    let sector_shift = u16_at(&data, 0x1E) as usize;
    let mini_shift = u16_at(&data, 0x20) as usize;
    // CFB v3/v4 use 512/4096-byte sectors and 64-byte mini sectors.
    // Reject invalid shifts before they can overflow or request huge offsets.
    if !matches!(sector_shift, 9 | 12) || mini_shift != 6 {
        return None;
    }
    let sector_size = 1usize << sector_shift;
    let mini_size = 1usize << mini_shift;
    let total_sectors = data.len().checked_sub(sector_size)? / sector_size;
    let n_fat = u32_at(&data, 0x2C) as usize;
    let dir_start = u32_at(&data, 0x30);
    let mini_cutoff = u32_at(&data, 0x38) as usize;
    let mini_fat_start = u32_at(&data, 0x3C);
    let n_mini_fat = u32_at(&data, 0x40) as usize;
    let difat_start = u32_at(&data, 0x44);
    let n_difat = u32_at(&data, 0x48) as usize;

    let sector = |i: u32| -> Option<&[u8]> {
        let index = usize::try_from(i).ok()?;
        if index >= total_sectors {
            return None;
        }
        let off = index.checked_add(1)?.checked_mul(sector_size)?;
        data.get(off..off.checked_add(sector_size)?)
    };

    // DIFAT：头部 109 项 + 后续 DIFAT 扇区
    let mut fat_sectors: Vec<u32> = Vec::new();
    for i in 0..109usize {
        if fat_sectors.len() >= n_fat {
            break;
        }
        let v = u32_at(&data, 0x4C + i * 4);
        if v != 0xFFFF_FFFF {
            fat_sectors.push(v);
        }
    }
    let mut d = difat_start;
    let mut difat_seen = vec![false; total_sectors];
    let mut guard = 0usize;
    let difat_limit = n_difat.min(total_sectors);
    while guard < difat_limit && d != 0xFFFF_FFFF {
        let index = usize::try_from(d).ok()?;
        let Some(seen) = difat_seen.get_mut(index) else {
            break;
        };
        if *seen {
            break;
        }
        *seen = true;
        guard += 1;
        let Some(sec) = sector(d) else { break };
        let per = sector_size / 4 - 1;
        for i in 0..per {
            let v = u32_at(sec, i * 4);
            if v != 0xFFFF_FFFF && fat_sectors.len() < n_fat {
                fat_sectors.push(v);
            }
        }
        d = u32_at(sec, per * 4);
    }

    let mut fat: Vec<u32> = Vec::new();
    for s in fat_sectors {
        let Some(sec) = sector(s) else { continue };
        for i in 0..sector_size / 4 {
            fat.push(u32_at(sec, i * 4));
        }
    }
    if fat.is_empty() {
        return None;
    }

    let read_chain = |start: u32, limit_secs: usize| -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur = start;
        let mut guard = 0usize;
        let mut seen = vec![false; total_sectors];
        let limit = limit_secs.min(total_sectors);
        while cur != 0xFFFF_FFFF && cur != 0xFFFF_FFFE && guard < limit {
            let Ok(index) = usize::try_from(cur) else { break };
            let Some(was_seen) = seen.get_mut(index) else {
                break;
            };
            if *was_seen {
                break;
            }
            *was_seen = true;
            let Some(sec) = sector(cur) else {
                break;
            };
            out.extend_from_slice(sec);
            cur = *fat.get(cur as usize).unwrap_or(&0xFFFF_FFFF);
            guard += 1;
        }
        out
    };

    // 目录项：128 字节一条，找根（类型 5）与它的 mini stream 起点
    let dir = read_chain(dir_start, total_sectors);
    let mut entries = Vec::new();
    let mut root = None;
    for i in 0..dir.len() / 128 {
        let e = &dir[i * 128..(i + 1) * 128];
        let name_len = u16_at(e, 0x40).min(64) as usize;
        let name: String = e[0..name_len.saturating_sub(2).max(0)]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .filter(|c| *c != 0)
            .map(|c| char::from_u32(c as u32).unwrap_or('?'))
            .collect();
        let kind = e[0x42];
        let first = u32_at(e, 0x74);
        let len = u64::from_le_bytes(e[0x78..0x80].try_into().ok()?) as u64;
        if kind == 5 {
            root = Some((first, len));
        }
        entries.push((name, kind, first, len));
    }

    let mini_stream = match root {
        Some((first, _)) => read_chain(first, total_sectors),
        None => Vec::new(),
    };
    let mut mini_fat = Vec::new();
    if n_mini_fat > 0 {
        let raw = read_chain(mini_fat_start, n_mini_fat);
        for i in 0..raw.len() / 4 {
            mini_fat.push(u32_at(&raw, i * 4));
        }
    }
    Some(Ole { data, sector_size, mini_size, mini_cutoff, fat, mini_fat, mini_stream, entries })
}

impl Ole {
    /// 读流：小于 mini_cutoff 的走 MiniFAT，否则走 FAT 链。
    fn stream(&self, name: &str, _max: usize) -> Option<Vec<u8>> {
        let (_, _, start, len) = self
            .entries
            .iter()
            .find(|e| e.1 == 2 && e.0.eq_ignore_ascii_case(name))?
            .clone();
        // `max` is a text-character limit, not an OLE stream-size limit.
        // Legacy Word commonly stores a multi-megabyte `WordDocument` stream
        // whose piece table points near the end; applying `max * 8` here
        // silently truncated those FC offsets and produced empty/partial text.
        // Keep the binary read bounded independently of the requested text.
        let len = usize::try_from(len).unwrap_or(64 << 20).min(64 << 20);
        let mut out = Vec::with_capacity(len.min(1 << 20));
        if len < self.mini_cutoff {
            let mut cur = start;
            let mut guard = 0usize;
            let mut seen = vec![false; self.mini_fat.len()];
            while cur != 0xFFFF_FFFF
                && cur != 0xFFFF_FFFE
                && guard < len / self.mini_size.max(1) + 2
            {
                let Ok(index) = usize::try_from(cur) else { break };
                let Some(was_seen) = seen.get_mut(index) else {
                    break;
                };
                if *was_seen {
                    break;
                }
                *was_seen = true;
                let Some(off) = index.checked_mul(self.mini_size) else {
                    break;
                };
                let Some(sec) = off
                    .checked_add(self.mini_size)
                    .and_then(|end| self.mini_stream.get(off..end))
                else {
                    break;
                };
                out.extend_from_slice(sec);
                cur = *self.mini_fat.get(index).unwrap_or(&0xFFFF_FFFF);
                guard += 1;
            }
        } else {
            let mut cur = start;
            let mut guard = 0usize;
            let mut seen = vec![false; self.fat.len()];
            while cur != 0xFFFF_FFFF && guard < len / self.sector_size.max(1) + 2 {
                let Ok(index) = usize::try_from(cur) else { break };
                let Some(was_seen) = seen.get_mut(index) else {
                    break;
                };
                if *was_seen {
                    break;
                }
                *was_seen = true;
                let Some(off) = index
                    .checked_add(1)
                    .and_then(|value| value.checked_mul(self.sector_size))
                else {
                    break;
                };
                let Some(sec) = off
                    .checked_add(self.sector_size)
                    .and_then(|end| self.data.get(off..end))
                else {
                    break;
                };
                out.extend_from_slice(sec);
                cur = *self.fat.get(index).unwrap_or(&0xFFFF_FFFF);
                guard += 1;
            }
        }
        out.truncate(len);
        Some(out)
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .unwrap_or(0)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .unwrap_or(0xFFFF_FFFF)
}

/// Word 97-2003：FIB → Clx → piece table（每块可能是 UTF-16 或单字节）。
fn ole_doc_text_with_cancel<F>(path: &Path, max: usize, cancelled: &F) -> String
where
    F: Fn() -> bool,
{
    if cancelled() {
        return String::new();
    }
    let Some(cf) = ole_open(path) else { return String::new() };
    if cancelled() {
        return String::new();
    }
    let Some(doc) = cf.stream("WordDocument", max) else { return String::new() };
    if u16_at(&doc, 0) != 0xA5EC {
        return String::new();
    }
    let table = if u16_at(&doc, 0x0A) & 0x0200 != 0 { "1Table" } else { "0Table" };
    let Some(tbl) = cf.stream(table, max) else { return String::new() };
    // fibRgFcLcb 起始 0x9A，fcClx 是第 34 项 → 0x9A + 33*8
    let mut fc = u32_at(&doc, 0x1A2) as usize;
    let mut lcb = u32_at(&doc, 0x1A6) as usize;
    if lcb == 0 || fc + lcb > tbl.len() {
        // 布局异常时按 FIB 里声明的 cbRgFcLcb 逐项找
        let cb = u16_at(&doc, 0x5A) as usize;
        if cb > 33 {
            let base = 0x9A;
            fc = u32_at(&doc, base + 33 * 8) as usize;
            lcb = u32_at(&doc, base + 33 * 8 + 4) as usize;
        }
    }
    if lcb == 0 || fc + lcb > tbl.len() {
        return String::new();
    }
    let clx = &tbl[fc..fc + lcb];
    let mut i = 0usize;
    while i + 3 <= clx.len() && clx[i] == 0x01 {
        i += 3 + u16_at(clx, i + 1) as usize;
    }
    if i + 5 > clx.len() || clx[i] != 0x02 {
        return String::new();
    }
    let plc_len = u32_at(clx, i + 1) as usize;
    let plc = &clx[i + 5..(i + 5 + plc_len).min(clx.len())];
    if plc.len() < 16 {
        return String::new();
    }
    let n = (plc.len() - 4) / 12;
    let mut out = String::new();
    let mut out_chars = 0usize;
    for k in 0..n {
        if cancelled() {
            return String::new();
        }
        let cp0 = u32_at(plc, k * 4) as usize;
        let cp1 = u32_at(plc, k * 4 + 4) as usize;
        let pcd = (n + 1) * 4 + k * 8;
        if pcd + 8 > plc.len() {
            break;
        }
        let fc_raw = u32_at(plc, pcd + 2);
        let len = cp1.saturating_sub(cp0);
        if fc_raw & 0x4000_0000 != 0 {
            let start = ((fc_raw & 0x3FFF_FFFF) / 2) as usize;
            if let Some(bytes) = doc.get(start..start + len) {
                let piece = decode_bytes(bytes);
                append_limited_chars(&mut out, &piece, &mut out_chars, max);
            }
        } else {
            let start = (fc_raw & 0x3FFF_FFFF) as usize;
            if let Some(bytes) = doc.get(start..start + len * 2) {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let piece = String::from_utf16_lossy(&units);
                append_limited_chars(&mut out, &piece, &mut out_chars, max);
            }
        }
        if out_chars >= max {
            break;
        }
    }
    if cancelled() {
        return String::new();
    }
    truncate_chars(&keep_printable(&out), max)
}

/// Excel 97-2003 (BIFF8)：SST 共享字符串 + 数字单元。
fn ole_xls_text(path: &Path, max: usize) -> String {
    let Some(cf) = ole_open(path) else { return String::new() };
    let Some(data) = cf.stream("Workbook", max).or_else(|| cf.stream("Book", max)) else {
        return String::new();
    };
    let mut sst: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut out_chars = 0usize;
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let typ = u16_at(&data, i);
        let len = u16_at(&data, i + 2) as usize;
        let body = &data[i + 4..(i + 4 + len).min(data.len())];
        match typ {
            0x00FC => parse_sst(&data, i + 4, len, &mut sst),
            0x00FD => {
                let idx = u32_at(body, 6) as usize;
                if let Some(s) = sst.get(idx) {
                    append_limited_chars(&mut out, s, &mut out_chars, max);
                    append_limited_chars(&mut out, "\t", &mut out_chars, max);
                }
            }
            // NUMBER / RK / MULRK 里的数值：表格正文往往就是这些数字
            0x0203 => append_limited_chars(
                &mut out,
                &format!("{}\t", f64_at(body, 6)),
                &mut out_chars,
                max,
            ),
            0x027E => append_limited_chars(
                &mut out,
                &format!("{}\t", rk_value(u32_at(body, 6))),
                &mut out_chars,
                max,
            ),
            0x000A => append_limited_chars(&mut out, "\n", &mut out_chars, max), // EOF
            _ => {}
        }
        i += 4 + len;
        if out_chars >= max {
            break;
        }
    }
    truncate_chars(&keep_printable(&out), max)
}

fn f64_at(b: &[u8], o: usize) -> f64 {
    b.get(o..o + 8)
        .map(|x| f64::from_le_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]]))
        .unwrap_or(0.0)
}

fn rk_value(raw: u32) -> f64 {
    let v = if raw & 0x02 != 0 {
        let n = (raw as i32) >> 2;
        n as f64
    } else {
        f64::from_bits(((raw & 0xFFFF_FFFC) as u64) << 32)
    };
    if raw & 0x01 != 0 {
        v / 100.0
    } else {
        v
    }
}

/// 解析 SST 记录（含其后的 CONTINUE）。跨记录断开的字符串直接跳过。
fn parse_sst(data: &[u8], start: usize, len: usize, out: &mut Vec<String>) {
    let end = (start + len).min(data.len());
    let mut p = start + 8; // 跳过 cstTotal / cstUnique
    while p + 3 <= end {
        let cch = u16_at(data, p) as usize;
        let flags = data[p + 2];
        let mut q = p + 3;
        let wide = flags & 0x01 != 0;
        let rich = flags & 0x08 != 0;
        let ext = flags & 0x04 != 0;
        let mut skip = if wide { cch * 2 } else { cch };
        if rich {
            skip += 2 + 4 * u16_at(data, q) as usize;
            q += 2;
        }
        if ext {
            skip += 4 + u32_at(data, q) as usize;
            q += 4;
        }
        if q + skip > end {
            // 跨 CONTINUE：放弃这条，跳到下一个记录边界由外层继续
            break;
        }
        let bytes = &data[q..q + skip];
        let s = if wide {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            decode_bytes(bytes)
        };
        out.push(s);
        p = q + skip;
        if out.len() > 200_000 {
            break;
        }
    }
}

/// PowerPoint 97-2003：遍历记录树，取 TextCharsAtom / TextBytesAtom。
fn ole_ppt_text(path: &Path, max: usize) -> String {
    let Some(cf) = ole_open(path) else { return String::new() };
    let Some(data) = cf.stream("PowerPoint Document", max) else { return String::new() };
    let mut out = String::new();
    let mut out_chars = 0usize;
    let mut stack: Vec<(usize, usize)> = vec![(0, data.len())];
    while let Some((mut p, end)) = stack.pop() {
        while p + 8 <= end {
            let ver_inst = u16_at(&data, p);
            let typ = u16_at(&data, p + 2);
            let len = u32_at(&data, p + 4) as usize;
            let body_start = p + 8;
            let body_end = (body_start + len).min(end);
            if body_end < body_start {
                break;
            }
            let container = ver_inst & 0x000F == 0x000F;
            match typ {
                0x0FA0 => {
                    let units: Vec<u16> = data[body_start..body_end]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let piece = String::from_utf16_lossy(&units);
                    append_limited_chars(&mut out, &piece, &mut out_chars, max);
                    append_limited_chars(&mut out, "\n", &mut out_chars, max);
                }
                0x0FA8 => {
                    let piece = decode_bytes(&data[body_start..body_end]);
                    append_limited_chars(&mut out, &piece, &mut out_chars, max);
                    append_limited_chars(&mut out, "\n", &mut out_chars, max);
                }
                _ => {}
            }
            if container && body_end > body_start {
                stack.push((body_start, body_end));
            }
            p = body_end;
            if out_chars >= max {
                break;
            }
        }
    }
    truncate_chars(&keep_printable(&out), max)
}

// ---------------------------------------------------------------- 自检
#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_text_fixture(content: &[u8]) -> (lopdf::Document, lopdf::ObjectId) {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        (doc, resources_id)
    }

    fn assert_pdf_fixture_extracts(mut document: lopdf::Document, expected: &str) {
        let path = std::env::temp_dir().join(format!(
            "deepdark-pdf-text-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        document.save(&path).unwrap();
        let passes_filter = pdf_has_text_operator(&path);
        let pages = extract_pages(&path, 10_000);
        std::fs::remove_file(path).unwrap();
        assert!(passes_filter, "valid text was rejected before extraction");
        let extracted = pages.unwrap().join(" ");
        assert!(extracted.contains(expected), "expected {expected:?}, got {extracted:?}");
    }

    #[test]
    fn compact_pdf_text_operators_reach_the_full_text_extractor() {
        for operation in [
            "(Piston)Tj",
            "[(Piston)]TJ",
            "(Piston)'",
            "0 0(Piston)\"",
        ] {
            let content = format!("BT /F1 12 Tf 20 700 Td 14 TL {operation} ET");
            let (document, _) = pdf_text_fixture(content.as_bytes());
            assert_pdf_fixture_extracts(document, "Piston");
        }
    }

    #[test]
    fn gbk_named_cmap_fallback_keeps_chinese_text_and_blank_page_positions() {
        use lopdf::{dictionary, Object, Stream};
        let (bytes, _, errors) = encoding_rs::GBK.encode("活塞 Engine123");
        assert!(!errors);
        let hex = bytes.iter().map(|byte| format!("{byte:02X}")).collect::<String>();
        let content = format!("BT /F1 12 Tf 20 700 Td <{hex}>Tj ET");
        let (mut doc, resources_id) = pdf_text_fixture(content.as_bytes());
        let font_id = doc.get_dictionary(resources_id).unwrap()
            .get(b"Font").unwrap().as_dict().unwrap()
            .get(b"F1").unwrap().as_reference().unwrap();
        let descriptor_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor", "FontName" => "SimHei", "Flags" => 4,
            "FontBBox" => vec![0.into(), (-200).into(), 1000.into(), 1000.into()],
            "ItalicAngle" => 0, "Ascent" => 880, "Descent" => -120, "CapHeight" => 880, "StemV" => 80,
        });
        let descendant_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "SimHei",
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal("GB1"), "Supplement" => 2,
            },
            "FontDescriptor" => descriptor_id, "DW" => 1000,
        });
        doc.objects.insert(font_id, Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "SimHei",
            "Encoding" => "GBK-EUC-H", "DescendantFonts" => vec![descendant_id.into()],
        }));
        let first_page = doc.page_iter().next().unwrap();
        let pages_id = doc.get_dictionary(first_page).unwrap().get(b"Parent").unwrap().as_reference().unwrap();
        let blank_content = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
        let blank_page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => blank_content,
        });
        let repeated_content = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let last_page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => repeated_content,
        });
        let pages = doc.get_dictionary_mut(pages_id).unwrap();
        pages.set("Kids", vec![first_page.into(), blank_page.into(), last_page.into()]);
        pages.set("Count", 3);

        let path = std::env::temp_dir().join(format!("deepdark-gbk-pdf-{}.pdf", std::process::id()));
        doc.save(&path).unwrap();
        let pages = extract_pages(&path, 10_000).unwrap();
        let limited = extract_pages(&path, 2).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(pages.len(), 3);
        assert!(pages[0].contains("活塞 Engine123"));
        assert!(pages[1].trim().is_empty());
        assert!(pages[2].contains("活塞 Engine123"));
        assert_eq!(limited, vec!["活塞", "", ""]);
    }

    #[test]
    fn gbk_fallback_rejects_invalid_source_bytes_and_does_not_guess_identity_encoding() {
        use lopdf::dictionary;
        let doc = lopdf::Document::with_version("1.5");
        assert!(!pdf_font_uses_gbk_source(&doc, &dictionary! { "Encoding" => "Identity-H" }));
        assert!(!pdf_font_uses_gbk_source(&doc, &dictionary! { "Encoding" => "Unknown-CMap" }));
        assert!(!pdf_font_uses_gbk_source(&doc, &dictionary! { "Encoding" => "GBK-EUC-H", "ToUnicode" => "Custom" }));
        let mut text = String::new();
        let mut used = 0;
        append_pdf_fallback_operands(
            &[lopdf::Object::string_literal(vec![0x81])],
            &PdfFallbackEncoding::Gbk, &mut text, &mut used, 100,
        );
        assert!(text.is_empty());
    }

    #[test]
    fn compressed_nested_form_text_uses_inherited_page_resources() {
        use lopdf::{dictionary, Stream};
        let (mut document, resources_id) = pdf_text_fixture(b"/Fm0 Do");
        let mut inner = Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }, "BT /F1 12 Tf 20 700 Td (Piston Form text)Tj ET\n".repeat(10).into_bytes());
        inner.compress().unwrap();
        assert!(inner.dict.get(b"Filter").is_ok());
        let inner_id = document.add_object(inner);
        let outer_id = document.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }, b"/Fm1 Do".to_vec()));
        document.get_dictionary_mut(resources_id).unwrap().set("XObject", dictionary! {
            "Fm0" => outer_id, "Fm1" => inner_id,
        });
        assert_pdf_fixture_extracts(document, "Piston Form text");
    }

    #[test]
    fn pdf_text_precheck_skips_known_image_content_and_fails_open_on_decode_errors() {
        use lopdf::{dictionary, Stream};
        let (mut document, resources_id) = pdf_text_fixture(b"% Tj TJ are comments\nq /Im0 Do Q");
        let image_id = document.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 1, "Height" => 1, "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        }, vec![0, 0, 0]));
        document.get_dictionary_mut(resources_id).unwrap().set("XObject", dictionary! {
            "Im0" => image_id,
        });
        assert!(!pdf_document_may_have_text(&document));

        let page_id = document.page_iter().next().unwrap();
        let content_id = document.get_page_contents(page_id)[0];
        let stream = document.get_object_mut(content_id).unwrap().as_stream_mut().unwrap();
        stream.dict.set("Filter", "FlateDecode");
        stream.set_content(b"invalid compressed bytes".to_vec());
        assert!(pdf_document_may_have_text(&document));
    }

    #[test]
    fn plain_exts_decode() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(decode_bytes(b"hello"), "hello");
    }

    #[test]
    fn legacy_text_append_obeys_character_limit_without_rescanning_output() {
        let mut output = String::new();
        let mut chars = 0;
        append_limited_chars(&mut output, "abc你好", &mut chars, 4);
        append_limited_chars(&mut output, "世界", &mut chars, 4);
        assert_eq!(output, "abc你");
        assert_eq!(chars, 4);

        append_limited_chars(&mut output, "世界", &mut chars, 6);
        assert_eq!(output, "abc你世界");
        assert_eq!(chars, 6);
    }

    #[test]
    fn printable_legacy_office_text_is_not_silently_truncated_at_four_thousand_chars() {
        let text = "发动机信息".repeat(1_000);
        assert_eq!(keep_printable(&text), text);
    }

    #[test]
    fn cancel_aware_extraction_stops_before_opening_the_file() {
        let path = Path::new("missing-test-document.doc");
        assert!(extract_text_with_cancel(path, 100, || true).is_none());
    }

    #[test]
    fn pdf_image_cache_accounts_for_decoded_raster_memory() {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(10, 20));
        assert_eq!(pdf_image_cache_cost(&[(1, image)]), 10 * 20 * 4);
    }

    #[test]
    fn pdf_image_sampling_targets_image_bearing_pages() {
        assert_eq!(evenly_sampled_indices(0, 64), Vec::<usize>::new());
        assert_eq!(evenly_sampled_indices(3, 64), vec![0, 1, 2]);
        assert_eq!(evenly_sampled_indices(100, 3), vec![0, 49, 99]);
        assert_eq!(evenly_sampled_indices(20, 1), vec![0]);
    }

    #[test]
    fn repeated_pdf_image_dimensions_require_three_adjacent_pages() {
        let watermark = (1200, 300);
        let diagram = (800, 600);
        let repeated = repeated_pdf_image_dimensions(&[
            vec![watermark, diagram],
            vec![diagram, watermark],
            vec![watermark],
            vec![diagram],
            vec![watermark],
            vec![watermark],
        ]);
        assert_eq!(
            repeated,
            [(0, watermark), (1, watermark), (2, watermark)]
                .into_iter()
                .collect()
        );

        assert!(repeated_pdf_image_dimensions(&[
            vec![watermark],
            vec![],
            vec![watermark],
            vec![watermark],
        ])
        .is_empty());
    }

    #[test]
    fn pdf_page_render_fallback_does_not_bypass_image_filters_or_sampling() {
        let watermark = (1200, 300);
        let ordinary = (800, 600);
        let page_dimensions = vec![
            vec![watermark],
            vec![watermark],
            vec![watermark],
            vec![ordinary],
            vec![],
        ];
        let repeated = repeated_pdf_image_dimensions(&page_dimensions);
        let selected = [3usize].into_iter().collect();
        assert_eq!(
            pdf_render_fallback_candidates(&page_dimensions, &repeated, &selected),
            vec![3, 4],
            "filtered repeated-image pages and unselected image pages must not be rendered back in"
        );
    }

    #[test]
    fn rtf_strips_controls() {
        assert_eq!(rtf_text_of(r"{\rtf1\ansi 你好\par 世界}"), "你好\n世界");
    }

    fn rtf_text_of(s: &str) -> String {
        let dir = std::env::temp_dir().join("deepdark-oracle-rtf-test.rtf");
        std::fs::write(&dir, s).unwrap();
        let t = rtf_text(&dir, 1000);
        let _ = std::fs::remove_file(&dir);
        t
    }
}
