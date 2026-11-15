//! 模型管理：扫描本地 GGUF、读取元信息、切换 / 导入 / 删除。
use crate::config::ModelConfig;
use crate::util::{format_ts, human_size};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize)]
pub struct ModelEntry {
    pub path: String,
    /// 展示名：优先取 GGUF 头里的 general.name，读不到就用文件名
    pub name: String,
    pub file: String,
    pub size: i64,
    pub size_label: String,
    pub mtime: i64,
    pub mtime_label: String,
    /// embedding | chat | projector | unknown
    pub kind: String,
    pub kind_label: String,
    /// 是否是当前加载的嵌入模型
    pub active: bool,
    /// `gguf` 或 `safetensors`。
    pub format: String,
    pub files_complete: bool,
    pub direct_loadable: bool,
    /// Whether the package looks like a supported OCR/vision-to-text model.
    /// This is a conservative hint used by the UI; the OCR runtime remains
    /// the final authority when the model is loaded.
    pub ocr_compatible: bool,
    pub model_dir: String,
    /// 模型本身声明的输入模态：text / image / audio / video。
    /// 这只描述权重能力；是否能在当前构建运行由 files_complete/direct_loadable 决定。
    pub modalities: Vec<String>,
    /// 可接手该模型的推理适配器（只表示候选，不表示扩展已安装）。
    pub candidate_backends: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelImportFile {
    pub path: String,
    pub relative_path: String,
    pub format: String,
    pub kind: String,
    pub kind_label: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelImportResult {
    pub source: String,
    pub destination: String,
    pub copied_files: usize,
    pub total_bytes: i64,
    pub files: Vec<ModelImportFile>,
    pub primary_model: String,
    pub primary_kind: String,
    pub files_complete: bool,
    pub direct_loadable: bool,
    pub needs_conversion: bool,
    pub warnings: Vec<String>,
    pub architecture: String,
    pub components: Vec<String>,
    /// 导入模型声明的输入模态：text / image / audio / video。
    pub modalities: Vec<String>,
    /// 可接手该模型的推理适配器（只表示候选，不表示扩展已安装）。
    pub candidate_backends: Vec<String>,
}

/// 从 Transformers/processor 配置中识别模型的模态塔。
///
/// safetensors 不包含统一的运行时入口；这里仅读取声明信息，避免把
/// 「权重里有视觉/音频塔」误报成「当前 llama.cpp 已经可以直接推理」。
fn detect_modalities(config: &str, processor: &str, fallback_text: bool) -> Vec<String> {
    let text = format!("{}\n{}", config, processor).to_lowercase();
    let mut out = Vec::with_capacity(4);
    let has = |needle: &str| text.contains(needle);
    if fallback_text
        || has("text_config")
        || has("language_model")
        || has("text_token")
        || has("tokenizer")
        || has("sentence_transformer")
    {
        out.push("text".to_string());
    }
    if has("vision_config")
        || has("vision_tower")
        || has("image_processor")
        || has("image_token_id")
        || has("image_seq_length")
    {
        out.push("image".to_string());
    }
    if has("audio_config")
        || has("audio_tower")
        || has("audio_processor")
        || has("audio_token_id")
        || has("feature_extractor_type")
    {
        out.push("audio".to_string());
    }
    if has("video_config")
        || has("video_processor")
        || has("video_token_id")
        || has("video_seq_length")
    {
        out.push("video".to_string());
    }
    out
}

/// 根据文件格式和架构声明选择候选后端。
///
/// 这里故意不把候选后端当成“已安装”：safetensors 只是权重容器，
/// 只有匹配的 Candle/Python 适配器实际加载成功后才能启用。
fn candidate_backends(format: &str, config: &str) -> Vec<String> {
    let cfg = config.to_lowercase();
    match format {
        "gguf" => vec!["llama.cpp".into()],
        "onnx" => vec!["onnxruntime".into()],
        "safetensors" if cfg.contains("embeddinggemma2model") || cfg.contains("embedding_gemma2") => {
            vec!["candle".into(), "python".into()]
        }
        "safetensors" => vec!["python".into()],
        _ => Vec::new(),
    }
}

/// Safetensors is only a weight container.  Do not offer every transformer
/// checkpoint as an OCR model: ordinary text embedding packages (including
/// EmbeddingGemma) cannot be loaded by the TeleOCR worker.  Keep this check
/// deliberately cheap and conservative; the worker performs the definitive
/// dependency/model validation later.
fn likely_ocr_model(file: &str, config: &str, processor: &str) -> bool {
    let hint = format!("{}\n{}\n{}", file, config, processor).to_ascii_lowercase();
    [
        "teleocr",
        "trocr",
        "ocr",
        "text_recognition",
        "vision_encoder_decoder",
        "vision2seq",
        "pix2struct",
    ]
    .iter()
    .any(|needle| hint.contains(needle))
}

/// Return the capabilities that can be declared cheaply while scanning a
/// model directory.  GGUF keeps the text tower and the multimodal projector
/// in separate files, so the GGUF weight itself cannot describe image/audio
/// support.  A sibling `mmproj` is the package-level signal; the native
/// llama.cpp loader still performs the authoritative capability check when the
/// model is activated.
fn gguf_modalities(path: &Path, kind: &str) -> Vec<String> {
    if kind != "embed" {
        return Vec::new();
    }
    let mut modalities = vec!["text".to_string()];
    let Some(parent) = path.parent() else {
        return modalities;
    };
    let has_projector = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let candidate = entry.path();
            candidate.is_file()
                && candidate
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case("gguf"))
                && candidate
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|value| {
                        let name = value.to_ascii_lowercase();
                        name.contains("mmproj") || name.contains("projector")
                    })
                    .unwrap_or(false)
        });
    if has_projector {
        // EmbeddingGemma 2's official projector carries the aligned vision
        // and audio towers.  Video is represented by sampled image frames in
        // mtmd, so it shares the vision capability. For other GGUF families,
        // runtime mtmd detection may narrow this declaration after loading.
        modalities.extend(["image".into(), "audio".into(), "video".into()]);
    }
    modalities
}

/// GGUF 头部里的显示名（只读前 256KB）。
fn gguf_name(path: &Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let mut head = vec![0u8; 256 * 1024];
    let Ok(n) = f.read(&mut head) else { return String::new() };
    head.truncate(n);
    if head.len() < 24 || &head[0..4] != b"GGUF" {
        return String::new();
    }
    let n_kv = u64::from_le_bytes(head[16..24].try_into().unwrap_or([0; 8])) as usize;
    let mut p = 24usize;
    let mut arch = String::new();
    let mut name = String::new();
    for _ in 0..n_kv.min(64) {
        let Some((key, np)) = read_str(&head, p) else { break };
        p = np;
        let Some(t) = head.get(p..p + 4) else { break };
        let vt = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
        p += 4;
        if vt == 8 {
            let Some((val, np)) = read_str(&head, p) else { break };
            p = np;
            match key.as_str() {
                "general.architecture" => arch = val,
                "general.name" => name = val,
                _ => {}
            }
        } else {
            // 其它类型：跳过数值 / 数组
            let skipped = match vt {
                0 | 1 | 7 => 1,
                2 | 3 => 2,
                4 | 5 | 6 => 4,
                10 | 11 | 12 => 8,
                9 => {
                    let Some(t) = head.get(p..p + 4) else { break };
                    let et = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
                    p += 4;
                    let Some(c) = head.get(p..p + 8) else { break };
                    let count = u64::from_le_bytes(c.try_into().unwrap_or([0; 8])) as usize;
                    p += 8;
                    let (ew, dynamic) = match et {
                        0 | 1 | 7 => (1usize, false),
                        2 | 3 => (2, false),
                        4 | 5 | 6 => (4, false),
                        10 | 11 | 12 => (8, false),
                        _ => (0, true),
                    };
                    if dynamic {
                        break;
                    }
                    let total = ew.saturating_mul(count.min(1 << 20));
                    if p + total > head.len() {
                        break;
                    }
                    total
                }
                _ => break,
            };
            if p + skipped > head.len() {
                break;
            }
            p += skipped;
        }
    }
    if name.is_empty() { arch } else { name }
}

fn read_str(buf: &[u8], p: usize) -> Option<(String, usize)> {
    let len_bytes = buf.get(p..p + 8)?;
    let len = u64::from_le_bytes(len_bytes.try_into().ok()?) as usize;
    let s = buf.get(p + 8..p + 8 + len.min(256))?;
    Some((String::from_utf8_lossy(s).to_string(), p + 8 + len))
}

fn classify(file: &str) -> (&'static str, &'static str) {
    let f = file.to_lowercase();
    if f.contains("mmproj") || f.contains("projector") || f.contains("vision") {
        ("projector", "视觉投影")
    } else if f.contains("embed") || f.contains("embedding") || f.contains("bge") || f.contains("gte") || f.contains("e5") || f.contains("gemma") || f.contains("bert") || f.contains("minilm") || f.contains("retriev") {
        ("embed", "嵌入模型")
    } else if f.contains("chat") || f.contains("instruct") || f.contains("qwen") || f.contains("llama-3") {
        ("chat", "对话模型")
    } else {
        ("unknown", "未知")
    }
}

fn classify_with_config(file: &str, config: &str) -> (&'static str, &'static str) {
    let filename = file.to_lowercase();
    let cfg = config.to_lowercase();
    // A projector is identified by its filename. Multimodal embedding
    // configs also contain `vision_config` and must not be classified as a
    // projector merely because they support images.
    if filename.contains("mmproj") || filename.contains("projector") {
        return ("projector", "视觉投影");
    }
    if cfg.contains("embeddinggemma2model") || cfg.contains("embedding_gemma2") {
        return ("embed", "嵌入模型");
    }
    classify(&filename)
}

fn supported_model_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    if matches!(name.as_str(), "modules.json" | "chat_template.jinja") {
        return true;
    }
    if name.ends_with(".safetensors.index.json") {
        return true;
    }
    if matches!(ext.as_str(), "gguf" | "safetensors") {
        return true;
    }
    if ext != "json" && ext != "model" && ext != "txt" {
        return false;
    }
    ["config", "tokenizer", "vocab", "merges", "special_tokens", "added_tokens", "sentencepiece", "preprocessor", "generation", "chat_template"]
        .iter()
        .any(|key| name.contains(key))
}

fn model_format(path: &Path) -> String {
    path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase()
}

fn model_files_complete(path: &Path, format: &str) -> bool {
    if format == "gguf" {
        return path.is_file();
    }
    if format != "safetensors" {
        return false;
    }
    let Some(dir) = path.parent() else { return false };
    crate::ai::safetensors_package_complete(dir)
}

fn candle_model_loadable(path: &Path, format: &str, embedding_gemma: bool) -> bool {
    format == "gguf"
        || (cfg!(feature = "candle-cuda")
            && embedding_gemma
            && path.parent().is_some_and(|dir| dir.join("tokenizer.json").is_file()))
}

fn model_file_kind(path: &Path, config: &str) -> (&'static str, &'static str) {
    classify_with_config(path.to_string_lossy().as_ref(), config)
}

/// 扫描模型目录里的 .gguf / .safetensors（最多递归四层，避免扫到大盘）。
pub fn scan(cfg: &ModelConfig, extra_dirs: &[PathBuf]) -> Vec<ModelEntry> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for d in extra_dirs {
        dirs.push(d.clone());
    }
    for p in [&cfg.embed_model, &cfg.chat_model] {
        if !p.trim().is_empty() {
            if let Some(parent) = Path::new(p).parent() {
                dirs.push(parent.to_path_buf());
            }
        }
    }
    dirs.sort();
    dirs.dedup();

    let mut out: Vec<ModelEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in dirs {
        for e in WalkDir::new(&d).follow_links(false).max_depth(4).into_iter().flatten() {
            let p = e.path();
            if !p.is_file() { continue; }
            let is_gguf = p
                .extension()
                .map(|x| x.eq_ignore_ascii_case("gguf"))
                .unwrap_or(false);
            let is_safetensors = p.extension().map(|x| x.eq_ignore_ascii_case("safetensors")).unwrap_or(false);
            if !is_gguf && !is_safetensors {
                continue;
            }
            let key = p.to_string_lossy().to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            let md = e.metadata().ok();
            let size = md.as_ref().map(|m| m.len() as i64).unwrap_or(0);
            let mtime = md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let file = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let config = p.parent().and_then(|dir| std::fs::read_to_string(dir.join("config.json")).ok()).unwrap_or_default();
            let processor = p
                .parent()
                .and_then(|dir| {
                    std::fs::read_to_string(dir.join("processor_config.json"))
                        .ok()
                        .or_else(|| std::fs::read_to_string(dir.join("preprocessor_config.json")).ok())
                })
                .unwrap_or_default();
            let (kind, kind_label) = model_file_kind(&p, &config);
            let display = if kind == "projector" { String::new() } else if is_gguf { gguf_name(&p) } else { p.parent().and_then(|x| x.file_name()).map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| file.trim_end_matches(".safetensors").to_string()) };
            let path_s = p.to_string_lossy().replace('/', "\\");
            let model_dir = p.parent().unwrap_or(&d).to_string_lossy().replace('/', "\\");
            let configured = norm(&cfg.embed_model);
            let active = !configured.is_empty()
                && (configured == norm(&path_s)
                    || (Path::new(&cfg.embed_model).is_dir()
                        && configured == norm(&model_dir)));
            let modalities = if is_gguf {
                gguf_modalities(p, kind)
            } else {
                detect_modalities(&config, &processor, false)
            };
            let candidate_backends = candidate_backends(
                if is_gguf { "gguf" } else { "safetensors" },
                &format!("{}\n{}", config, processor),
            );
            let embedding_gemma = config.to_ascii_lowercase().contains("embeddinggemma2model")
                || config.to_ascii_lowercase().contains("embedding_gemma2");
            let ocr_compatible = is_safetensors
                && likely_ocr_model(&file, &config, &processor);
            out.push(ModelEntry {
                name: if display.is_empty() { file.clone() } else { display },
                path: path_s,
                file,
                size,
                size_label: human_size(size),
                mtime,
                mtime_label: format_ts(mtime),
                kind: kind.to_string(),
                kind_label: kind_label.to_string(),
                active,
                format: if is_gguf { "gguf".into() } else { "safetensors".into() },
                files_complete: model_files_complete(&p, if is_gguf { "gguf" } else { "safetensors" }),
                direct_loadable: model_files_complete(&p, if is_gguf { "gguf" } else { "safetensors" })
                    && candle_model_loadable(&p, if is_gguf { "gguf" } else { "safetensors" }, embedding_gemma),
                ocr_compatible,
                model_dir,
                modalities,
                candidate_backends,
            });
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size));
    out
}

/// 便携版 / 安装版首次运行时 `model.embed_model` 是空的。这里在
/// exe 同级的 `models\` 与数据目录的 `models\` 里挑一个能直接加载的嵌入模型，
/// 让「装完就能用」成立，而不是逼用户先去设置页填路径。
///
/// 优先多模态 safetensors（自带视觉/音频塔），其次任意可直接加载的嵌入 GGUF。
/// 只返回路径，是否落盘由调用方决定。
pub fn autodetect_embed_model(cfg: &ModelConfig, app_dir: &Path, data_dir: &Path) -> Option<String> {
    for dir in [app_dir.join("models"), data_dir.join("models")] {
        if !dir.is_dir() {
            continue;
        }
        let entries = scan(cfg, std::slice::from_ref(&dir));
        let found = entries
            .iter()
            .find(|m| {
                m.kind == "embed"
                    && m.format == "safetensors"
                    && m.modalities.iter().any(|x| matches!(x.as_str(), "image" | "audio" | "video"))
            })
            .or_else(|| entries.iter().find(|m| m.direct_loadable && m.kind == "embed"));
        if let Some(found) = found {
            return Some(found.path.clone());
        }
    }
    None
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").to_lowercase()
}

/// 删除模型文件。正在使用的模型不允许删，避免程序下次启动找不到文件。
pub fn remove(path: &str, cfg: &ModelConfig) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("文件不存在：{path}"));
    }
    if norm(&cfg.embed_model) == norm(path) {
        return Err("这是当前正在使用的嵌入模型，请先切换到其它模型再删除".into());
    }
    if norm(&cfg.chat_model) == norm(path) {
        return Err("这是当前配置的对话模型，请先在设置里清空后再删除".into());
    }
    let ext_ok = p
        .extension()
        .map(|x| x.eq_ignore_ascii_case("gguf") || x.eq_ignore_ascii_case("safetensors"))
        .unwrap_or(false);
    if !ext_ok {
        return Err("只允许删除 .gguf 或 .safetensors 模型文件".into());
    }
    // Keep user data recoverable: Windows sends model files to the Recycle
    // Bin instead of permanently unlinking them.
    #[cfg(windows)]
    {
        let script = "param([string]$Path); $ErrorActionPreference='Stop'; Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($Path, 'OnlyErrorDialogs', 'SendToRecycleBin')";
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
            .arg(p)
            .output()
            .map_err(|e| format!("打开回收站操作失败：{e}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if detail.is_empty() { "移动到回收站失败".into() } else { format!("移动到回收站失败：{detail}") });
        }
    }
    #[cfg(not(windows))]
    std::fs::remove_file(p).map_err(|e| format!("删除失败：{e}"))?;
    Ok(())
}

/// 把外部 GGUF 复制进模型目录（重名自动加序号）。
pub fn import(src: &str, dir: &Path) -> Result<String, String> {
    let s = Path::new(src);
    if !s.is_file() {
        return Err(format!("文件不存在：{src}"));
    }
    let is_gguf = s
        .extension()
        .map(|x| x.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false);
    if !is_gguf {
        return Err("只能导入 .gguf 模型文件".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("创建模型目录失败：{e}"))?;
    let file = s.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
    let dest = crate::util::unique_path(&dir.join(file));
    std::fs::copy(s, &dest).map_err(|e| format!("复制失败：{e}"))?;
    Ok(dest.to_string_lossy().replace('/', "\\"))
}

/// 导入一个模型目录。只复制模型权重和常见 tokenizer/config 配套文件，避免把缓存、日志等无关文件带进应用。
/// 权重文件采用流式 `std::fs::copy`，不会把数 GB 模型读入内存。
pub fn import_folder(src: &str, dir: &Path) -> Result<ModelImportResult, String> {
    let source = Path::new(src);
    if !source.is_dir() { return Err(format!("文件夹不存在：{src}")); }
    let source_leaf = source.file_name().and_then(|x| x.to_str()).filter(|x| !x.is_empty()).unwrap_or("imported-model");
    let source_name = if matches!(source_leaf.to_lowercase().as_str(), "model" | "models" | "weights" | "checkpoint") {
        source.parent().and_then(|p| p.file_name()).and_then(|x| x.to_str()).filter(|x| !x.is_empty()).unwrap_or(source_leaf)
    } else {
        source_leaf
    };
    let destination = crate::util::unique_path(&dir.join(source_name));
    std::fs::create_dir_all(&destination).map_err(|e| format!("创建模型目录失败：{e}"))?;

    let mut candidates: Vec<(PathBuf, PathBuf, String)> = Vec::new();
    let mut config_by_dir = std::collections::HashMap::<PathBuf, String>::new();
    for entry in WalkDir::new(source).follow_links(false).max_depth(4).into_iter().flatten() {
        let p = entry.path();
        if !p.is_file() { continue; }
        if p.file_name().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("config.json")).unwrap_or(false) {
            if let Ok(mut c) = std::fs::read_to_string(p) {
                // WalkDir 的遍历顺序不作为契约；把同目录 processor 配置一起读入，
                // 即便它在 config.json 之前被遍历到也不会被覆盖。
                if let Some(parent) = p.parent() {
                    for extra in ["processor_config.json", "preprocessor_config.json"] {
                        if let Ok(x) = std::fs::read_to_string(parent.join(extra)) {
                            c.push('\n');
                            c.push_str(&x);
                        }
                    }
                }
                config_by_dir.insert(p.parent().unwrap_or(source).to_path_buf(), c);
            }
        } else if p
            .file_name()
            .and_then(|x| x.to_str())
            .map(|x| matches!(x.to_ascii_lowercase().as_str(), "processor_config.json" | "preprocessor_config.json"))
            .unwrap_or(false)
        {
            // 把 processor 声明并入同目录配置，供模态检测使用。
            if let Ok(c) = std::fs::read_to_string(p) {
                let slot = config_by_dir.entry(p.parent().unwrap_or(source).to_path_buf()).or_default();
                if !slot.is_empty() { slot.push('\n'); }
                slot.push_str(&c);
            }
        }
        if supported_model_file(p) {
            let rel = p.strip_prefix(source).unwrap_or(p).to_path_buf();
            candidates.push((p.to_path_buf(), rel, String::new()));
        }
    }
    let mut files = Vec::with_capacity(candidates.len());
    let mut primary: Option<(PathBuf, String, String, i64)> = None;
    let mut warnings = Vec::new();
    let mut architecture = String::new();
    let mut components: Vec<String> = Vec::new();
    let mut modalities: Vec<String> = Vec::new();
    let mut backends: Vec<String> = Vec::new();
    for (candidate, rel, _) in &candidates {
        let rel_lower = rel.to_string_lossy().to_lowercase();
        let cfg = config_by_dir.get(candidate.parent().unwrap_or(source)).map(String::as_str).unwrap_or("").to_lowercase();
        let is_weight = candidate.extension().map(|x| x.eq_ignore_ascii_case("gguf") || x.eq_ignore_ascii_case("safetensors")).unwrap_or(false);
        if is_weight {
            let format = candidate.extension().and_then(|x| x.to_str()).unwrap_or_default().to_ascii_lowercase();
            let kind = classify_with_config(&candidate.to_string_lossy(), &cfg).0;
            let detected = if format == "gguf" {
                gguf_modalities(candidate, kind)
            } else {
                detect_modalities(&cfg, "", false)
            };
            for modality in detected {
                if !modalities.iter().any(|m| m == &modality) { modalities.push(modality); }
            }
            for backend in candidate_backends(
                &candidate.extension().and_then(|x| x.to_str()).unwrap_or_default().to_lowercase(),
                &cfg,
            ) {
                if !backends.iter().any(|x| x == &backend) {
                    backends.push(backend);
                }
            }
        }
        if candidate.extension().map(|x| x.eq_ignore_ascii_case("gguf") || x.eq_ignore_ascii_case("safetensors")).unwrap_or(false) {
            if cfg.contains("embeddinggemma2model") || cfg.contains("embedding_gemma2") {
                architecture = "EmbeddingGemma2Model".into();
                if !components.iter().any(|x| x == "文本编码器") { components.push("文本编码器".into()); }
            } else if architecture.is_empty() && (cfg.contains("architectures") || cfg.contains("model_type")) {
                architecture = "Transformers / SentenceTransformers".into();
            }
        }
        if rel_lower == "modules.json" && !components.iter().any(|x| x == "SentenceTransformer pipeline") {
            components.push("SentenceTransformer pipeline".into());
        }
        if rel_lower.contains("1_pooling") && !components.iter().any(|x| x == "SentenceTransformer pooling") {
            components.push("SentenceTransformer pooling".into());
        }
        if rel_lower.contains("2_normalize") && !components.iter().any(|x| x == "归一化层") {
            components.push("归一化层".into());
        }
    }
    for (modality, label) in [("text", "文本编码器"), ("image", "图像编码器"), ("audio", "音频编码器"), ("video", "视频处理器")] {
        if modalities.iter().any(|m| m == modality) && !components.iter().any(|x| x == label) {
            components.push(label.to_string());
        }
    }
    let copy_result = (|| -> Result<(), String> {
        for (src_file, rel, _) in candidates {
            let parent_cfg = config_by_dir.get(src_file.parent().unwrap_or(source)).map(String::as_str).unwrap_or("");
            let (kind, kind_label) = model_file_kind(&src_file, parent_cfg);
            let format = model_format(&src_file);
            let cfg_lower = parent_cfg.to_lowercase();
            let detected = if format == "gguf" {
                gguf_modalities(&src_file, kind)
            } else {
                detect_modalities(&cfg_lower, "", false)
            };
            for modality in detected {
                if !modalities.iter().any(|m| m == &modality) { modalities.push(modality); }
            }
            if architecture.is_empty() {
                if cfg_lower.contains("embeddinggemma2model") || cfg_lower.contains("embedding_gemma2") {
                    architecture = "EmbeddingGemma2Model".into();
                    components = vec!["文本编码器".into(), "SentenceTransformer pooling".into(), "归一化层".into()];
                } else if cfg_lower.contains("architectures") || cfg_lower.contains("model_type") {
                    architecture = "Transformers / SentenceTransformers".into();
                }
            }
            let rel_lower = rel.to_string_lossy().to_lowercase();
            if rel_lower.contains("1_pooling") && !components.iter().any(|x| x == "SentenceTransformer pooling") {
                components.push("SentenceTransformer pooling".into());
            }
            if rel_lower.contains("2_normalize") && !components.iter().any(|x| x == "归一化层") {
                components.push("归一化层".into());
            }
            let size = std::fs::metadata(&src_file).map(|m| m.len() as i64).unwrap_or(0);
            let dest_file = destination.join(&rel);
            if let Some(parent) = dest_file.parent() { std::fs::create_dir_all(parent).map_err(|e| format!("创建模型子目录失败：{e}"))?; }
            std::fs::copy(&src_file, &dest_file).map_err(|e| format!("复制模型文件失败 {}：{e}", src_file.display()))?;
            let rel_s = rel.to_string_lossy().replace('/', "\\");
            let dest_s = dest_file.to_string_lossy().replace('/', "\\");
            files.push(ModelImportFile { path: dest_s.clone(), relative_path: rel_s.clone(), format: format.clone(), kind: kind.into(), kind_label: kind_label.into(), size });
            let is_weight = format == "gguf" || format == "safetensors";
            // `mmproj` is often larger than the text GGUF (for example, the
            // EmbeddingGemma projector is close to 1 GB). It is a companion
            // encoder and must never win the primary-model selection by size.
            let prefer_this = kind != "projector" && primary.as_ref().map(|x| {
                let current_is_gguf = x.2 == "gguf";
                let candidate_is_gguf = format == "gguf";
                candidate_is_gguf && !current_is_gguf
                    || candidate_is_gguf == current_is_gguf && size > x.3
            }).unwrap_or(true);
            if is_weight && prefer_this {
                primary = Some((dest_file, kind.into(), format, size));
            }
        }
        Ok(())
    })();
    if let Err(error) = copy_result {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(error);
    }
    if files.is_empty() {
        let _ = std::fs::remove_dir_all(&destination);
        return Err("所选文件夹内没有找到 .gguf 或 .safetensors 模型文件".into());
    }
    let Some((primary_path, primary_kind, primary_format, _)) = primary else {
        let _ = std::fs::remove_dir_all(&destination);
        return Err("没有可识别的模型权重文件".into());
    };
    let files_complete = model_files_complete(&primary_path, &primary_format);
    let direct_loadable = files_complete
        && candle_model_loadable(
            &primary_path,
            &primary_format,
            architecture == "EmbeddingGemma2Model",
        );
    if !direct_loadable {
        if !files_complete {
            warnings.push("已识别 safetensors 权重，但模型目录缺少 config/tokenizer/权重分片文件。".into());
        } else if backends.iter().any(|backend| backend == "candle") {
            warnings.push("模型文件完整；当前构建未启用 Candle CUDA 推理后端。".into());
        } else {
            warnings.push("已识别 safetensors 权重，但当前架构没有已集成的原生推理后端。".into());
        }
        if architecture == "EmbeddingGemma2Model" {
            let detail = if modalities.iter().any(|m| m == "image") || modalities.iter().any(|m| m == "audio") {
                "，配置还声明了图像、音频和视频能力；完整多模态运行需要对应的配套组件"
            } else {
                "（SentenceTransformer 编码器 + pooling + normalize）"
            };
            warnings.push(format!("检测到 EmbeddingGemma2Model{detail}；当前构建不能直接激活此 safetensors。"));
        }
    }
    if files.iter().any(|f| f.format == "gguf") && files.iter().any(|f| f.format == "safetensors") {
        warnings.push("同时发现 GGUF 和 safetensors，应用会优先使用 GGUF；safetensors 作为配套源文件保留。".into());
    }
    Ok(ModelImportResult {
        source: source.to_string_lossy().replace('/', "\\"),
        destination: destination.to_string_lossy().replace('/', "\\"),
        copied_files: files.len(),
        total_bytes: files.iter().map(|f| f.size).sum(),
        files,
        primary_model: primary_path.to_string_lossy().replace('/', "\\"),
        primary_kind,
        direct_loadable,
        needs_conversion: !direct_loadable && !backends.iter().any(|backend| backend == "candle"),
        files_complete,
        warnings,
        architecture,
        components,
        modalities,
        candidate_backends: backends,
    })
}

/// 模型目录：exe 同级 models/，不存在就建。
pub fn model_dir(app_dir: &Path, data_dir: &Path) -> PathBuf {
    let a = app_dir.join("models");
    if a.exists() {
        return a;
    }
    let d = data_dir.join("models");
    if d.exists() {
        return d;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::{candidate_backends, candle_model_loadable, classify_with_config, detect_modalities, model_files_complete, supported_model_file};
    use std::path::{Path, PathBuf};

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deepaisee-model-check-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recognizes_embedding_gemma_safetensors_from_config() {
        let (kind, label) = classify_with_config(
            "model.safetensors",
            r#"{"architectures":["EmbeddingGemma2Model"],"model_type":"embedding_gemma2"}"#,
        );
        assert_eq!(kind, "embed");
        assert_eq!(label, "嵌入模型");
    }

    #[test]
    fn multimodal_embedding_config_is_not_a_projector() {
        let (kind, _) = classify_with_config(
            "model.safetensors",
            r#"{"architectures":["EmbeddingGemma2Model"],"vision_config":{"model_type":"gemma4_vision"}}"#,
        );
        assert_eq!(kind, "embed");
        let (projector, _) = classify_with_config("mmproj-embeddinggemma-2-BF16.gguf", "{}");
        assert_eq!(projector, "projector");
    }

    #[test]
    fn keeps_safetensors_shard_index_and_sentence_transformer_metadata() {
        assert!(supported_model_file(Path::new("model.safetensors.index.json")));
        assert!(supported_model_file(Path::new("modules.json")));
        assert!(supported_model_file(Path::new("tokenizer.json")));
    }

    #[test]
    fn detects_embedding_gemma_multimodal_towers() {
        let config = r#"{
            "architectures":["EmbeddingGemma2Model"],
            "text_config":{"model_type":"embedding_gemma2_text"},
            "vision_config":{"model_type":"gemma4_vision"},
            "audio_config":{"model_type":"gemma4_audio"},
            "video_token_id":258884
        }"#;
        let modalities = detect_modalities(config, "video_processor", false);
        assert_eq!(modalities, ["text", "image", "audio", "video"]);
    }

    #[test]
    fn selects_backend_candidates_without_claiming_installation() {
        assert_eq!(candidate_backends("gguf", ""), ["llama.cpp"]);
        assert_eq!(candidate_backends("onnx", ""), ["onnxruntime"]);
        assert_eq!(
            candidate_backends("safetensors", r#"{"model_type":"embedding_gemma2"}"#),
            ["candle", "python"]
        );
        assert_eq!(candidate_backends("safetensors", "{}"), ["python"]);
    }

    #[test]
    fn checks_safetensors_package_files_independently_from_backend_support() {
        let dir = fixture_dir("single");
        let weights = dir.join("model.safetensors");
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(&weights, "weights").unwrap();
        assert!(model_files_complete(&weights, "safetensors"));

        std::fs::remove_file(dir.join("tokenizer.json")).unwrap();
        assert!(!model_files_complete(&weights, "safetensors"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accepts_descriptive_single_safetensors_filename() {
        let dir = fixture_dir("descriptive-single");
        let weights = dir.join("embeddinggemma-2-bf16.safetensors");
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(&weights, "weights").unwrap();
        assert!(model_files_complete(&weights, "safetensors"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checks_every_safetensors_shard_from_the_index() {
        let dir = fixture_dir("shards");
        let first = dir.join("model-00001-of-00002.safetensors");
        let second = dir.join("model-00002-of-00002.safetensors");
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.model"), "tokenizer").unwrap();
        std::fs::write(&first, "one").unwrap();
        std::fs::write(&second, "two").unwrap();
        std::fs::write(
            dir.join("model.safetensors.index.json"),
            r#"{"weight_map":{"a":"model-00001-of-00002.safetensors","b":"model-00002-of-00002.safetensors"}}"#,
        ).unwrap();
        assert!(model_files_complete(&first, "safetensors"));

        std::fs::remove_file(second).unwrap();
        assert!(!model_files_complete(&first, "safetensors"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_shard_index_overrides_a_single_file_looking_checkpoint() {
        let dir = fixture_dir("invalid-index");
        let weights = dir.join("model.safetensors");
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(&weights, "weights").unwrap();
        std::fs::write(
            dir.join("model.safetensors.index.json"),
            r#"{"weight_map":{"a":"model-00001-of-00002.safetensors","b":"model-00002-of-00002.safetensors"}}"#,
        )
        .unwrap();
        assert!(!model_files_complete(&weights, "safetensors"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_candle_loadability_requires_the_fast_tokenizer() {
        let dir = fixture_dir("fast-tokenizer");
        let weights = dir.join("model.safetensors");
        std::fs::write(&weights, "weights").unwrap();
        std::fs::write(dir.join("tokenizer.model"), "sentencepiece").unwrap();
        assert!(!candle_model_loadable(&weights, "safetensors", true));

        std::fs::write(dir.join("tokenizer.json"), "{} ").unwrap();
        assert_eq!(
            candle_model_loadable(&weights, "safetensors", true),
            cfg!(feature = "candle-cuda")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
