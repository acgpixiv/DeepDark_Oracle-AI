//! 配置管理：读写 data/config.json，提供默认值。
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct ScanConfig {
    pub roots: Vec<String>,
    /// Roots kept in the directory list but excluded from indexing.
    #[serde(default)]
    pub disabled_roots: Vec<String>,
    pub exclude_dirs: Vec<String>,
    /// Explicit files/directories skipped by the scanner. Paths are compared
    /// case-insensitively after normalisation; empty keeps legacy behaviour.
    #[serde(default)]
    pub exclude_paths: Vec<String>,
    /// Optional per-extension allow-list discovered by the UI. Empty means
    /// all extensions in the selected category are accepted.
    #[serde(default)]
    pub enabled_exts: Vec<String>,
    #[serde(default)]
    pub known_exts: Vec<String>,
    #[serde(default)]
    pub filter_extensions: bool,
    #[serde(default)]
    pub root_extensions: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub root_enabled_exts: std::collections::BTreeMap<String, Vec<String>>,
    pub follow_links: bool,
    pub max_size_mb: u64,
    pub min_size_kb: u64,
    pub images: bool,
    pub videos: bool,
    pub audios: bool,
    /// Whether PDF files are included. Kept separate because PDFs may contain
    /// both extracted text and embedded images.
    #[serde(default = "default_true")]
    pub pdf: bool,
    pub documents: bool,
    pub archives: bool,
    pub others: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: vec![],
            disabled_roots: vec![],
            exclude_dirs: vec![
                "$RECYCLE.BIN".into(),
                "System Volume Information".into(),
                "node_modules".into(),
                ".git".into(),
                "AppData".into(),
                "Windows".into(),
                "Program Files".into(),
                "Program Files (x86)".into(),
                "ProgramData".into(),
            ],
            exclude_paths: Vec::new(),
            enabled_exts: Vec::new(),
            known_exts: Vec::new(),
            filter_extensions: false,
            root_extensions: std::collections::BTreeMap::new(),
            root_enabled_exts: std::collections::BTreeMap::new(),
            follow_links: false,
            max_size_mb: 4096,
            min_size_kb: 0,
            images: true,
            videos: true,
            audios: true,
            pdf: true,
            documents: true,
            archives: true,
            others: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct IndexConfig {
    /// Requested index dimension exposed in expert settings. Runtime use must
    /// be paired with a model-specific projection; changing it alone cannot
    /// preserve embedding geometry.
    #[serde(default = "default_vector_dim")]
    pub vector_dim: usize,
    pub max_text_chars: usize,
    pub auto_fulltext: bool,
    pub auto_embed: bool,
    pub image_max_side: u32,
    /// EmbeddingGemma2 image soft-token budget (70..1120).
    #[serde(default = "default_image_token_budget")]
    pub image_token_budget: usize,
    /// Soft-token budget for raster images extracted from PDFs. Keeping this
    /// separate avoids paying the standalone-photo budget for every page.
    #[serde(default = "default_pdf_image_token_budget")]
    pub pdf_image_token_budget: usize,
    /// EmbeddingGemma2 video-frame soft-token budget (70..1120).
    #[serde(default = "default_video_token_budget")]
    pub video_token_budget: usize,
    /// Load the vision tower for image/video embeddings.
    #[serde(default = "default_true")]
    pub vision_encoder: bool,
    /// Load the audio tower for audio embeddings.
    #[serde(default = "default_true")]
    pub audio_encoder: bool,
    pub video_frames: u32,
    /// 视频抽帧的时间间隔（秒）。EmbeddingGemma2 默认 1 FPS；
    /// 最大帧数仍会限制长视频的总成本。
    pub video_frame_interval: f32,
    /// 单个视频最多保留多少帧（安全上限，超过时自动放稀采样）。
    pub video_max_frames: u32,
    /// 相邻两帧的图像指纹（dHash）汉明距离小于该值即视为重复画面丢弃。
    pub video_dedup_distance: u32,
    /// 是否对视频音轨做语义（CLAP）索引，用于「用文字描述找声音」。
    pub video_index_audio: bool,
    pub audio_seconds: f32,
    pub batch_size: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            vector_dim: 768,
            max_text_chars: 200_000,
            auto_fulltext: true,
            auto_embed: true,
            image_max_side: 448,
            image_token_budget: 1120,
            pdf_image_token_budget: 280,
            video_token_budget: 140,
            vision_encoder: true,
            audio_encoder: true,
            video_frames: 16,
            video_frame_interval: 1.0,
            video_max_frames: 64,
            video_dedup_distance: 6,
            video_index_audio: true,
            // A video window is encoded as 30 seconds of mono 16 kHz audio
            // together with its 1 FPS frames in the shared 8K context.
            audio_seconds: 30.0,
            batch_size: 8,
        }
    }
}

fn default_image_token_budget() -> usize { 1120 }
fn default_pdf_image_token_budget() -> usize { 280 }
fn default_video_token_budget() -> usize { 140 }
fn default_vector_dim() -> usize { 768 }

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct ModelConfig {
    /// GGUF 嵌入模型路径
    pub embed_model: String,
    /// Last explicitly selected embedding model, retained when deactivated
    /// so an indexing run can reload it lazily.
    #[serde(default)]
    pub last_embed_model: String,
    /// GGUF 生成模型路径（可选，用于 RAG 问答）
    pub chat_model: String,
    pub n_threads: i32,
    /// Selected accelerator device id/name. `auto` chooses the largest available GPU.
    #[serde(default = "default_gpu_device")]
    pub gpu_device: String,
    pub context_size: u32,
    pub auto_load: bool,
    pub embedding_dim: usize,
    /// 跨模态模型（中文 CLIP）目录，空 = 自动在 data/models/clip 与 exe/models/clip 找
    pub clip_dir: String,
    /// 关掉后不做「用文字描述找图片」，省内存
    pub clip_enabled: bool,
    /// 跨模态音频模型（CLAP）目录，空 = 自动在 data/models/clap 与 exe/models/clap 找
    pub clap_dir: String,
    /// 关掉后不做「用文字描述找声音」
    pub clap_enabled: bool,
    /// Optional TeleOCR model directory. TeleOCR and its Python dependencies
    /// are user-provided and are never bundled with the lightweight installer.
    #[serde(default)]
    pub teleocr_model_dir: String,
    /// Python executable used by the optional TeleOCR worker. Empty means
    /// resolve `python` from PATH.
    #[serde(default)]
    pub teleocr_python_path: String,
    /// Optional TeleOCR source checkout root, used when not installed as a
    /// Python package.
    #[serde(default)]
    pub teleocr_code_dir: String,
    /// Single-model CPU batching policy. `eco` uses at most one third of the
    /// logical CPUs, `auto` uses up to 90% while respecting current load, and
    /// `custom` uses the requested thread count.
    #[serde(default = "default_parallel_mode")]
    pub parallel_mode: String,
    /// Number of CPU worker threads when `parallel_mode = custom`.
    /// The old `parallel_instances` JSON key is accepted for migration.
    #[serde(default = "default_parallel_threads", alias = "parallel_instances")]
    pub parallel_threads: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            embed_model: String::new(),
            last_embed_model: String::new(),
            chat_model: String::new(),
            n_threads: 0,
            // A llama model load requires a selected accelerator and always
            // offloads all model layers; it never falls back to CPU inference.
            gpu_device: "auto".into(),
            // EmbeddingGemma2 uses one shared 8192-token budget across text,
            // image, video and audio placeholders.
            context_size: 8192,
            auto_load: false,
            embedding_dim: 768,
            clip_dir: String::new(),
            clip_enabled: true,
            clap_dir: String::new(),
            clap_enabled: true,
            teleocr_model_dir: String::new(),
            teleocr_python_path: String::new(),
            teleocr_code_dir: String::new(),
            parallel_mode: "auto".into(),
            parallel_threads: 4,
        }
    }
}

fn default_gpu_device() -> String {
    "auto".into()
}

fn default_parallel_mode() -> String {
    "auto".into()
}

fn default_parallel_threads() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct SearchConfig {
    pub page_size: usize,
    pub debounce_ms: u64,
    pub use_pinyin: bool,
    pub use_fuzzy: bool,
    pub use_semantic: bool,
    pub use_fulltext: bool,
    pub semantic_weight: f32,
    pub semantic_floor: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            // Keep the default useful for large local indexes. Individual
            // callers can still request a smaller page explicitly.
            page_size: 50_000,
            debounce_ms: 200,
            use_pinyin: true,
            use_fuzzy: true,
            // Hybrid search keeps lexical lookup fast and loads the embedding
            // model lazily only when a query is submitted.
            use_semantic: true,
            use_fulltext: true,
            semantic_weight: 0.55,
            semantic_floor: 0.45,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct WatchConfig {
    pub enabled: bool,
    pub auto_index: bool,
    pub debounce_secs: u64,
    #[serde(default)]
    pub roots: Vec<String>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self { enabled: false, auto_index: true, debounce_secs: 2, roots: Vec::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct UiConfig {
    pub font_size: u32,
    /// light | dark | system
    pub theme: String,
    /// zh | en
    pub language: String,
    pub remember_window: bool,
    /// 开机自启（写入当前用户的 Run 注册表项）
    pub autostart: bool,
    /// 启动时自动扫描索引根目录的增量变化
    pub auto_scan_on_start: bool,
    /// 启动扫描发现变化时是否在底部给出提示条
    pub notify_file_changes: bool,
    /// 默认搜索模式：fulltext | semantic | hybrid
    pub default_search_mode: String,
    /// Shows advanced indexing and model controls only after confirmation.
    #[serde(default)]
    pub professional_mode: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_size: 14,
            theme: "system".into(),
            language: "zh".into(),
            remember_window: true,
            autostart: false,
            auto_scan_on_start: false,
            notify_file_changes: true,
            // FTS5/BM25 is the instant, model-free startup path. Users can
            // switch to semantic or hybrid explicitly when needed.
            default_search_mode: "hybrid".into(),
            professional_mode: false,
        }
    }
}

/// 工作区：主页可以直接指定一个目录，搜索时把该目录里**尚未入库**的文件
/// 实时抽取正文一起参与检索，不影响索引数据库。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// 工作区根目录列表，空表示未启用（可同时指定多个目录）
    pub dirs: Vec<String>,
    /// Recently selected search paths. The home page keeps at most three
    /// entries here; permanently configured paths live in `pinned_paths`.
    #[serde(default)]
    pub recent_paths: Vec<String>,
    /// Paths retained by the user and shown in the settings page. Unlike
    /// recent paths this list has no UI-imposed limit.
    #[serde(default)]
    pub pinned_paths: Vec<String>,
    /// Startup search scope: remember | all | indexed | fixed.
    #[serde(default = "default_workspace_startup")]
    pub startup_scope: String,
    /// Used when `startup_scope = fixed`.
    #[serde(default)]
    pub startup_path: String,
    /// Last path/scope selected on the home page. This is written only after
    /// a valid user selection, so a removed drive can safely fall back.
    #[serde(default)]
    pub last_path: String,
    /// Last selected scope (`all`, `indexed`, or `path`). Kept separately
    /// from the path because `all` and `indexed` do not have a directory.
    #[serde(default = "default_workspace_scope")]
    pub last_scope: String,
    pub enabled: bool,
    /// 是否对未入库文件做实时正文抽取（否则只按文件名匹配）
    pub live_extract: bool,
    /// 实时抽取时跳过的超大文件（MB）
    pub max_file_mb: u64,
    /// 递归深度上限，0 = 不递归
    pub depth: u32,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            dirs: vec![],
            recent_paths: vec![],
            pinned_paths: vec![],
            startup_scope: "remember".into(),
            startup_path: String::new(),
            last_path: String::new(),
            last_scope: "indexed".into(),
            enabled: true,
            live_extract: true,
            max_file_mb: 64,
            depth: 4,
        }
    }
}

fn default_workspace_startup() -> String { "remember".into() }
fn default_workspace_scope() -> String { "indexed".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
// 缺字段时用 Default 补齐，避免用户手改配置时整份失效
#[serde(default)]
pub struct ServerConfig {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
    pub token: String,
    #[serde(default)]
    pub allow_write: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { enabled: false, bind: "0.0.0.0".into(), port: 8770, token: String::new(), allow_write: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
// 允许用户手写只含部分字段的 config.json（缺的段用默认值），
// 否则少一个字段整份配置都会静默回退到默认值。
#[serde(default)]
pub struct AppConfig {
    /// Configuration schema marker used for one-time migrations.
    #[serde(default = "current_config_version")]
    pub config_version: u32,
    pub scan: ScanConfig,
    pub index: IndexConfig,
    pub model: ModelConfig,
    pub search: SearchConfig,
    pub watch: WatchConfig,
    pub ui: UiConfig,
    pub workspace: WorkspaceConfig,
    pub server: ServerConfig,
}

const CURRENT_CONFIG_VERSION: u32 = 3;

fn current_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

impl AppConfig {
    /// 从磁盘加载；文件不存在或损坏时回退默认值。
    pub fn load(path: &Path) -> Self {
        let Some(text) = std::fs::read_to_string(path).ok() else {
            return Self { config_version: CURRENT_CONFIG_VERSION, ..Self::default() };
        };
        let has_version = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("config_version").cloned())
            .is_some();
        let mut config: Self = serde_json::from_str(&text).unwrap_or_default();

        // Legacy configs (including early numbered schemas) could enable an
        // implicit startup scan. Keep startup side-effect free after this
        // one-time migration while allowing users to enable it later in UI.
        if !has_version || config.config_version < CURRENT_CONFIG_VERSION {
            config.config_version = CURRENT_CONFIG_VERSION;
            config.ui.auto_scan_on_start = false;
            config.model.auto_load = false;
            // Older releases enabled semantic search from the home page. The
            // current layered design starts with FTS5 and only wakes AI for
            // an explicit semantic request.
            config.search.use_semantic = true;
            config.ui.default_search_mode = "hybrid".into();
            if let Ok(normalized) = serde_json::to_string_pretty(&config) {
                let _ = std::fs::write(path, normalized);
            }
        }
        if !matches!(config.index.vector_dim, 256 | 512 | 768) {
            config.index.vector_dim = 768;
        }
        config
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, text)
    }
}

/// 数据目录解析：优先 exe 同级 data/（便携模式）；如果该位置不可写
/// （典型场景：NSIS 安装到 Program Files），回退到用户可写目录
/// %LOCALAPPDATA%\DeepDark_Oracle\data，保证索引库/配置/模型缓存总能落盘。
static DATA_DIR_CACHE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn data_dir(app_dir: Option<&Path>) -> PathBuf {
    DATA_DIR_CACHE.get_or_init(|| resolve_data_dir(app_dir)).clone()
}

fn resolve_data_dir(app_dir: Option<&Path>) -> PathBuf {
    let portable = match app_dir {
        Some(d) => d.join("data"),
        None => PathBuf::from("data"),
    };
    let local_fallback = std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("DeepDark_Oracle").join("data"));
    let roaming_fallback = std::env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("DeepDark_Oracle").join("data"));
    let fallback = local_fallback
        .clone()
        .or_else(|| roaming_fallback.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("DeepDark_Oracle").join("data"));

    // Updates can leave a newly created, empty database beside the executable
    // while the previous index remains in a user data directory. Inspect all
    // known locations together so one stale portable shell cannot win.
    let candidates = [Some(portable.clone()), local_fallback, roaming_fallback]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if let Some(existing) = select_populated_data_dir(&candidates) {
        return existing;
    }
    if portable.is_dir() {
        return portable;
    }
    // 目录还不存在：试着建一次并写探针，判断这个位置是否可写。
    if std::fs::create_dir_all(&portable).is_ok() {
        let probe = portable.join(".write-probe");
        let ok = std::fs::write(&probe, b"ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        if ok {
            return portable;
        }
    }
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

fn select_populated_data_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .filter_map(|dir| data_dir_quality(dir).map(|quality| (dir, quality)))
        .max_by_key(|(_, quality)| *quality)
        .map(|(dir, _)| dir.clone())
}

fn data_dir_quality(dir: &Path) -> Option<(bool, u128)> {
    let db_path = dir.join("index.db");
    if !db_path.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let has_files_table = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='files' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !has_files_table {
        return None;
    }
    let has_rows = conn
        .query_row("SELECT 1 FROM files LIMIT 1", [], |_| Ok(()))
        .is_ok();
    drop(conn);

    let bytes = [
        "index.db", "index.db-wal", "index.db-shm",
        "vectors/vectors.bin", "vectors/ids.bin", "vectors/meta.json",
        "vectors_media/vectors.bin", "vectors_media/ids.bin", "vectors_media/meta.json",
        "vectors_clip/vectors.bin", "vectors_clip/ids.bin", "vectors_clip/meta.json",
        "vectors_audio/vectors.bin", "vectors_audio/ids.bin", "vectors_audio/meta.json",
    ]
    .iter()
    .map(|name| std::fs::metadata(dir.join(name)).map(|m| m.len() as u128).unwrap_or(0))
    .sum();
    Some((has_rows, bytes))
}

pub fn config_path(app_dir: Option<&Path>) -> PathBuf {
    data_dir(app_dir).join("config.json")
}

/// 插件扩展预留目录：exe 同级 plugins/。安装版卸载时会保留该目录，
/// 后续插件只需把目录（或 zip）放进去即可被发现。
pub fn plugins_dir(app_dir: &Path) -> PathBuf {
    let dir = app_dir.join("plugins");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[cfg(test)]
mod data_dir_tests {
    use super::select_populated_data_dir;
    use std::path::PathBuf;

    #[test]
    fn populated_index_wins_over_an_empty_update_shell() {
        let base = std::env::temp_dir().join(format!(
            "deepdark-data-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let portable = base.join("portable");
        let fallback = base.join("fallback");
        std::fs::create_dir_all(&portable).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        for dir in [&portable, &fallback] {
            let conn = rusqlite::Connection::open(dir.join("index.db")).unwrap();
            conn.execute("CREATE TABLE files (id INTEGER PRIMARY KEY)", []).unwrap();
        }
        let conn = rusqlite::Connection::open(fallback.join("index.db")).unwrap();
        conn.execute("INSERT INTO files (id) VALUES (1)", []).unwrap();
        drop(conn);

        assert_eq!(
            select_populated_data_dir(&[portable.clone(), fallback.clone()]),
            Some(fallback)
        );
        let _ = std::fs::remove_dir_all(PathBuf::from(base));
    }
}
