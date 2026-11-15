//! 应用全局状态。
use crate::ai::Embedder;
use crate::config::AppConfig;
use crate::db::Db;
use crate::index::Indexer;
use crate::search::SearchEngine;
use crate::server::LanServer;
use crate::vector::VectorStore;
use crate::watch::FileWatcher;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub struct AppState {
    pub cfg: RwLock<AppConfig>,
    pub db: Arc<Db>,
    /// 文本向量空间（llama.cpp GGUF 512 维 / hash 256 维）
    pub vectors: RwLock<Arc<VectorStore>>,
    /// 图片/音频/视频向量空间（hash 特征 256 维，与文本空间独立）
    pub media_vectors: RwLock<Arc<VectorStore>>,
    /// 跨模态图片空间（中文 CLIP 512 维）：图片与视频关键帧，可用文字检索
    pub clip_vectors: RwLock<Arc<VectorStore>>,
    /// 跨模态音频空间（CLAP 512 维）：音频与视频音轨，可用文字检索
    pub audio_vectors: RwLock<Arc<VectorStore>>,
    pub embedder: RwLock<Arc<dyn Embedder>>,
    /// 媒体兜底编码器（始终是 hash）
    pub media_embedder: RwLock<Arc<dyn Embedder>>,
    /// 中文 CLIP（可选，缺模型时为 None）
    pub clip: RwLock<Option<Arc<crate::ai::clip::ClipModel>>>,
    /// CLAP 音频-文本对齐（可选）
    pub clap: RwLock<Option<Arc<crate::ai::clap::ClapModel>>>,
    pub indexer: RwLock<Option<Arc<Indexer>>>,
    pub watcher: Arc<FileWatcher>,
    pub server: Arc<LanServer>,
    pub data_dir: PathBuf,
    pub app_dir: PathBuf,
    pub scan_stats: Arc<crate::scan::ScanStatsShared>,
    pub scan_cancel: Arc<std::sync::atomic::AtomicBool>,
    pub scan_busy: Arc<std::sync::atomic::AtomicBool>,
    /// Blocks new scans while an embedding/OCR model is being disabled.
    pub model_transition: std::sync::atomic::AtomicBool,
    /// Serializes model replacement with the final state swap during disable.
    pub model_change_lock: Mutex<()>,
    /// Saved indexing settings are applied at the next idle task boundary,
    /// never by replacing the worker in the middle of an indexing pass.
    pub index_settings_dirty: std::sync::atomic::AtomicBool,
    /// Model construction settings need a lazy reload before the next vector
    /// task. Text-only indexing can apply its settings without waking the GPU.
    pub model_settings_dirty: std::sync::atomic::AtomicBool,
    /// 工作区实时检索（未入库文件的现场抽取 + 缓存）
    pub workspace: crate::workspace::WorkspaceSearch,
    /// Short-lived query embedding cache. The search page can request several
    /// retrieval lanes for the same text at once; reusing the exact vector
    /// avoids serializing duplicate model forwards on the GPU.
    pub query_vectors: Mutex<HashMap<String, Vec<f32>>>,
    /// In-flight query embeddings keyed by the same model/query identity as
    /// `query_vectors`. Concurrent semantic/image/audio lanes rendezvous on
    /// one result instead of issuing duplicate GPU forwards on a cold cache.
    pub query_vector_inflight:
        Mutex<HashMap<String, Arc<OnceLock<Result<Vec<f32>, String>>>>>,
    /// Very short-lived complete-result cache. Search lanes and pagination
    /// often repeat the same vector/FTS scan while the UI settles filters;
    /// retaining a small result set for a fraction of a second removes that
    /// duplicate work without making index updates appear stale.
    pub search_cache: Mutex<HashMap<String, (Instant, Vec<crate::search::Hit>)>>,
}

pub struct EngineParts {
    pub db: Arc<Db>,
    pub vectors: Arc<VectorStore>,
    pub media: Arc<VectorStore>,
    pub clip: Arc<VectorStore>,
    pub audio: Arc<VectorStore>,
    pub cfg: crate::config::SearchConfig,
}

fn search_cache_key(
    q: &crate::search::SearchQuery,
    model: &str,
    vector_dim: usize,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    q.text.trim().hash(&mut hasher);
    q.kinds.hash(&mut hasher);
    q.exts.hash(&mut hasher);
    q.roots.hash(&mut hasher);
    q.categories.hash(&mut hasher);
    q.size_min.hash(&mut hasher);
    q.size_max.hash(&mut hasher);
    q.mtime_after.hash(&mut hasher);
    q.mtime_before.hash(&mut hasher);
    q.limit.hash(&mut hasher);
    q.offset.hash(&mut hasher);
    q.sort.hash(&mut hasher);
    q.desc.hash(&mut hasher);
    q.use_fts.hash(&mut hasher);
    q.use_pinyin.hash(&mut hasher);
    q.use_fuzzy.hash(&mut hasher);
    q.use_semantic.hash(&mut hasher);
    q.search_mode.hash(&mut hasher);
    q.cross_modal.hash(&mut hasher);
    q.workspace.hash(&mut hasher);
    q.scope.hash(&mut hasher);
    q.scope_path.hash(&mut hasher);
    q.vector_space.hash(&mut hasher);
    q.vector_model_key.hash(&mut hasher);
    if let Some(vector) = &q.query_vector {
        vector.len().hash(&mut hasher);
        for value in vector.iter().step_by(16) {
            value.to_bits().hash(&mut hasher);
        }
    }
    model.hash(&mut hasher);
    vector_dim.hash(&mut hasher);
    format!("{model}|{vector_dim}|{:016x}", hasher.finish())
}

/// 加载跨模态模型（CLIP / CLAP）。
///
/// 两个模型都是可选的：没装就返回 None，检索时静默跳过跨模态段。
/// 加载失败也不能让主程序起不来——用户可能只想用文件名搜索。
pub fn load_cross_modal(
    cfg: &crate::config::ModelConfig,
    app_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> (
    Option<Arc<crate::ai::clip::ClipModel>>,
    Option<Arc<crate::ai::clap::ClapModel>>,
) {
    let threads = if cfg.n_threads > 0 { cfg.n_threads } else { 0 };
    let clip = crate::ai::clip::resolve_dir(cfg, app_dir, data_dir)
        .and_then(|d| crate::ai::clip::ClipModel::load(&d, threads))
        .map(Arc::new);
    let clap = crate::ai::clap::resolve_dir(cfg, app_dir, data_dir)
        .and_then(|d| crate::ai::clap::ClapModel::load(&d, threads))
        .map(Arc::new);
    (clip, clap)
}

impl AppState {
    pub fn new(app_dir: PathBuf) -> Result<Self, String> {
        // 注意：这里必须传 config.json 的完整路径，不是目录。
        // 传目录时 read_to_string 永远失败 → 整份配置静默回退默认值，
        // 表现为「配置文件里明明有模型路径，程序却当没配」。
        let cfg = AppConfig::load(&crate::config::config_path(Some(&app_dir)));
        let data_dir = crate::config::data_dir(Some(&app_dir));
        std::fs::create_dir_all(&data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
        let db = Arc::new(Db::open(&data_dir.join("index.db")).map_err(|e| format!("打开数据库失败: {e}"))?);
        db.migrate().map_err(|e| format!("数据库迁移失败: {e}"))?;
        db.sync_index_roots(&cfg.scan.roots, &cfg.scan.disabled_roots)
            .map_err(|e| format!("同步索引目录失败: {e}"))?;
        let vec_dir = data_dir.join("vectors");
        // 先用默认的 hash 后端建 embedder，向量库维度跟随后端实际输出，
        // 不能直接用 config 里的 embedding_dim（那可能是 llama 的 512 或旧的 768）。
        let embedder: Arc<dyn Embedder> = Arc::new(crate::ai::hash::HashEmbedder::new());
        let media_embedder: Arc<dyn Embedder> = Arc::new(crate::ai::hash::HashEmbedder::new());
        let store_dim = configured_vector_dim(cfg.index.vector_dim);
        let stale_vector_layout = [
            data_dir.join("vectors"),
            data_dir.join("vectors_media"),
            data_dir.join("vectors_clip"),
            data_dir.join("vectors_audio"),
        ]
        .iter()
        .any(|dir| crate::vector::saved_dim_of(dir).is_some_and(|saved| saved != store_dim));
        let vectors = Arc::new(VectorStore::open(&vec_dir, store_dim).map_err(|e| format!("向量库打开失败: {e}"))?);
        // 媒体空间固定 256 维（HashEmbedder 的输出），与文本空间分开存
        let media_vectors = Arc::new(
            VectorStore::open(&data_dir.join("vectors_media"), store_dim)
                .map_err(|e| format!("媒体向量库打开失败: {e}"))?,
        );
        // 跨模态空间：CLIP 图片 512 维、CLAP 音频 512 维。
        // 与上面两个空间物理隔离——不同模型的向量放在一起会互相干扰。
        let clip_vectors = Arc::new(
            VectorStore::open(&data_dir.join("vectors_clip"), store_dim)
                .map_err(|e| format!("跨模态图片向量库打开失败: {e}"))?,
        );
        let audio_vectors = Arc::new(
            VectorStore::open(&data_dir.join("vectors_audio"), store_dim)
                .map_err(|e| format!("跨模态音频向量库打开失败: {e}"))?,
        );
        // Video windows are checkpointed in SQLite together with their
        // embedding payload. Restore durable rows before creating the
        // indexer, so an interrupted run remains searchable after restart.
        match db.restore_video_segment_vectors(&vectors, &media_vectors) {
            Ok(restored) if restored > 0 => {
                let _ = db.log(
                    "model",
                    &format!("startup restored {restored} persisted video segment vectors"),
                );
            }
            Ok(_) => {}
            Err(error) => {
                let _ = db.log(
                    "error",
                    &format!("video segment vector restore failed at startup: {error}"),
                );
            }
        }
        if stale_vector_layout {
            let _ = db.reset_states("embed");
            let _ = db.log(
                "model",
                &format!("检测到旧向量库维度，当前索引库统一为 {store_dim} 维；旧文件保留，需重新建立索引"),
            );
        }
        // Cross-modal encoders are loaded lazily with an explicit indexing
        // request. Opening the search UI must remain lightweight.
        let (clip, clap) = (None, None);
        Ok(Self {
            cfg: RwLock::new(cfg),
            db,
            vectors: RwLock::new(vectors),
            media_vectors: RwLock::new(media_vectors),
            clip_vectors: RwLock::new(clip_vectors),
            audio_vectors: RwLock::new(audio_vectors),
            embedder: RwLock::new(embedder),
            media_embedder: RwLock::new(media_embedder),
            clip: RwLock::new(clip),
            clap: RwLock::new(clap),
            indexer: RwLock::new(None),
            watcher: Arc::new(FileWatcher::new()),
            server: Arc::new(LanServer::new()),
            data_dir,
            app_dir,
            scan_stats: Arc::new(crate::scan::ScanStatsShared::new()),
            scan_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scan_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            model_transition: std::sync::atomic::AtomicBool::new(false),
            model_change_lock: Mutex::new(()),
            index_settings_dirty: std::sync::atomic::AtomicBool::new(false),
            model_settings_dirty: std::sync::atomic::AtomicBool::new(false),
            workspace: crate::workspace::WorkspaceSearch::new(),
            query_vectors: Mutex::new(HashMap::new()),
            query_vector_inflight: Mutex::new(HashMap::new()),
            search_cache: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn clear_search_cache(&self) {
        self.search_cache.lock().clear();
    }

    pub fn search_engine(&self) -> EngineParts {
        let cfg = self.cfg.read().search.clone();
        EngineParts {
            db: Arc::clone(&self.db),
            vectors: Arc::clone(&self.vectors.read()),
            media: Arc::clone(&self.media_vectors.read()),
            clip: Arc::clone(&self.clip_vectors.read()),
            audio: Arc::clone(&self.audio_vectors.read()),
            cfg,
        }
    }

    /// Reuse text queries encoded by optional cross-modal models.  The search
    /// page can issue image/audio lanes concurrently and users often toggle a
    /// lane or scope repeatedly for the same text.  Without this small cache
    /// each toggle reruns an ONNX forward (and briefly contends on the model's
    /// session mutex), even though the query vector is unchanged.
    pub(crate) fn cached_aux_query<F>(&self, key: String, make: F) -> Result<Vec<f32>, String>
    where
        F: FnOnce() -> Result<Vec<f32>, String>,
    {
        if let Some(cached) = self.query_vectors.lock().get(&key).cloned() {
            return Ok(cached);
        }
        let (flight, owner, cached) = {
            let mut flights = self.query_vector_inflight.lock();
            if let Some(existing) = flights.get(&key) {
                (Some(Arc::clone(existing)), false, None)
            } else if let Some(cached) = self.query_vectors.lock().get(&key).cloned() {
                (None, false, Some(cached))
            } else {
                let created = Arc::new(OnceLock::new());
                flights.insert(key.clone(), Arc::clone(&created));
                (Some(created), true, None)
            }
        };
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let flight = flight.expect("auxiliary query flight is initialized");
        let result = flight.get_or_init(|| {
            let result = make();
            if let Ok(encoded) = &result {
                let mut cache = self.query_vectors.lock();
                if cache.len() >= 256 {
                    if let Some(oldest) = cache.keys().next().cloned() {
                        cache.remove(&oldest);
                    }
                }
                cache.insert(key.clone(), encoded.clone());
            }
            result
        });
        let result = result.clone();
        if owner {
            self.query_vector_inflight.lock().remove(&key);
        }
        result
    }

    /// Load only the dedicated cross-modal encoder required by an explicit
    /// image/audio search. These models stay unloaded at application startup
    /// and are released after indexing, so relying on `model_load_inner` to
    /// initialize them leaves the search fallback without a query vector.
    pub fn ensure_cross_modal_for_search(&self, image: bool, audio: bool) -> Result<(), String> {
        let configured = self.cfg.read().model.clone();
        let clip_ready = !image || (configured.clip_enabled && self.clip.read().is_some());
        let clap_ready = !audio || (configured.clap_enabled && self.clap.read().is_some());
        let clip_stale = image && !configured.clip_enabled && self.clip.read().is_some();
        let clap_stale = audio && !configured.clap_enabled && self.clap.read().is_some();
        if clip_ready && clap_ready && !clip_stale && !clap_stale {
            return Ok(());
        }
        if self.model_transition.load(std::sync::atomic::Ordering::Acquire) {
            return Err("模型正在切换，请稍后重试图像或音频检索".into());
        }

        // Serialize with model unload/replacement so a just-loaded search
        // encoder cannot be cleared between initialization and query encode.
        let _model_change = self.model_change_lock.lock();
        if self.model_transition.load(std::sync::atomic::Ordering::Acquire) {
            return Err("模型正在切换，请稍后重试图像或音频检索".into());
        }
        let cfg = self.cfg.read().model.clone();
        // A stale runtime must never make a disabled extension appear usable.
        if image && !cfg.clip_enabled {
            *self.clip.write() = None;
        }
        if audio && !cfg.clap_enabled {
            *self.clap.write() = None;
        }
        let need_clip = image && cfg.clip_enabled && self.clip.read().is_none();
        let need_clap = audio && cfg.clap_enabled && self.clap.read().is_none();
        if !need_clip && !need_clap {
            if let Some(indexer) = self.indexer() {
                indexer.set_cross_modal(self.clip.read().clone(), self.clap.read().clone());
            }
            if (image && !cfg.clip_enabled) || (audio && !cfg.clap_enabled) {
                return Err("所需跨模态模型已停用".into());
            }
            return Ok(());
        }
        if need_clip {
            let dir = crate::ai::clip::resolve_dir(&cfg, &self.app_dir, &self.data_dir)
                .ok_or_else(|| {
                    if cfg.clip_enabled {
                        "未找到 CLIP 图片检索模型；请配置 models/clip 或设置模型目录".to_string()
                    } else {
                        "CLIP 图片检索模型已停用".to_string()
                    }
                })?;
            let clip = crate::ai::clip::ClipModel::load(
                &dir,
                if cfg.n_threads > 0 { cfg.n_threads } else { 0 },
            )
            .ok_or_else(|| {
                format!(
                    "CLIP 图片检索模型加载失败：{}（请检查模型文件和 ONNX Runtime）",
                    dir.display()
                )
            })?;
            *self.clip.write() = Some(Arc::new(clip));
        }
        if need_clap {
            let dir = crate::ai::clap::resolve_dir(&cfg, &self.app_dir, &self.data_dir)
                .ok_or_else(|| {
                    if cfg.clap_enabled {
                        "未找到 CLAP 音频检索模型；请配置 models/clap 或设置模型目录".to_string()
                    } else {
                        "CLAP 音频检索模型已停用".to_string()
                    }
                })?;
            let clap = crate::ai::clap::ClapModel::load(
                &dir,
                if cfg.n_threads > 0 { cfg.n_threads } else { 0 },
            )
            .ok_or_else(|| {
                format!(
                    "CLAP 音频检索模型加载失败：{}（请检查模型文件和 ONNX Runtime）",
                    dir.display()
                )
            })?;
            *self.clap.write() = Some(Arc::new(clap));
        }
        if let Some(indexer) = self.indexer() {
            indexer.set_cross_modal(self.clip.read().clone(), self.clap.read().clone());
        }
        Ok(())
    }

    /// Reconcile already-loaded CLIP/CLAP runtimes with saved model settings.
    /// Newly enabled extensions remain lazy; a loaded runtime is reloaded only
    /// when its own configuration changes, and disabled extensions are always
    /// removed from both application and indexer state.
    pub fn sync_cross_modal_config(
        &self,
        previous: &crate::config::ModelConfig,
        current: &crate::config::ModelConfig,
    ) {
        let threads = if current.n_threads > 0 { current.n_threads } else { 0 };

        let clip_config_changed = previous.clip_enabled != current.clip_enabled
            || previous.clip_dir != current.clip_dir
            || previous.n_threads != current.n_threads;
        if !current.clip_enabled {
            *self.clip.write() = None;
        } else if clip_config_changed && self.clip.read().is_some() {
            let next = crate::ai::clip::resolve_dir(current, &self.app_dir, &self.data_dir)
                .and_then(|dir| crate::ai::clip::ClipModel::load(&dir, threads))
                .map(Arc::new);
            if next.is_none() {
                let _ = self.db.log(
                    "error",
                    "CLIP 配置已更改，但新模型未能加载；图像跨模态检索已停用",
                );
            }
            *self.clip.write() = next;
        }

        let clap_config_changed = previous.clap_enabled != current.clap_enabled
            || previous.clap_dir != current.clap_dir
            || previous.n_threads != current.n_threads;
        if !current.clap_enabled {
            *self.clap.write() = None;
        } else if clap_config_changed && self.clap.read().is_some() {
            let next = crate::ai::clap::resolve_dir(current, &self.app_dir, &self.data_dir)
                .and_then(|dir| crate::ai::clap::ClapModel::load(&dir, threads))
                .map(Arc::new);
            if next.is_none() {
                let _ = self.db.log(
                    "error",
                    "CLAP 配置已更改，但新模型未能加载；音频跨模态检索已停用",
                );
            }
            *self.clap.write() = next;
        }

        if let Some(indexer) = self.indexer() {
            indexer.set_cross_modal(self.clip.read().clone(), self.clap.read().clone());
        }
    }

    pub fn search(&self, q: &mut crate::search::SearchQuery) -> Vec<crate::search::Hit> {
        // Own the scope string before mutating the query below. Borrowing it
        // directly from `q` would keep an immutable borrow alive across the
        // root-scope normalization and prevent the required mutable borrow.
        let scope = q.scope.as_deref().unwrap_or("legacy").to_owned();
        let scan = self.cfg.read().scan.clone();
        if q.vector_space.as_deref() == Some("shared") {
            let mut cfg = self.cfg.read().index.clone();
            cfg.vector_dim = self.vectors.read().dim();
            let embedder = self.embedder.read();
            q.vector_model_key = Some(crate::index::embedder_model_key(embedder.as_ref(), &cfg));
        }
        // `all` intentionally bypasses the enabled-root restriction. The
        // indexed database can safely search every row, while `indexed` keeps
        // the configured-root behaviour and `path` narrows to the selected
        // directory (including a directory that has not been indexed yet).
        if scope == "path" {
            if let Some(path) = q.scope_path.as_deref().map(crate::util::norm_path).filter(|p| !p.is_empty()) {
                q.roots = vec![path];
            }
        } else if scope == "indexed" || scope == "legacy" {
            crate::search::apply_enabled_root_scope(q, &scan.roots, &scan.disabled_roots);
        } else {
            q.roots.clear();
        }
        let cache_key = search_cache_key(q, &self.embedder.read().info().model, self.vectors.read().dim());
        let cached = { self.search_cache.lock().get(&cache_key).cloned() };
        if let Some((created, hits)) = cached {
            // Keep a warm result for a few seconds while the search page is
            // settling filters and pagination.  Index/model mutations call
            // `clear_search_cache`, so this longer TTL cannot expose stale
            // rows after an index update or model replacement.
            if created.elapsed() <= Duration::from_secs(3) {
                return hits;
            }
            self.search_cache.lock().remove(&cache_key);
        }
        // 跨模态：把查询词编码成图片/声音空间里的向量。
        // 模型没装时静默跳过，用户仍然能靠文件名/标签搜到东西。
        let mode_wants_image = q.search_mode.as_deref().is_some_and(|mode| mode.eq_ignore_ascii_case("image"));
        let mode_wants_audio = q.search_mode.as_deref().is_some_and(|mode| mode.eq_ignore_ascii_case("audio"));
        if q.cross_modal.unwrap_or(mode_wants_image || mode_wants_audio)
            && q.text.chars().count() >= 2
            // A shared EmbeddingGemma query is already being searched in the
            // primary multimodal vector store.  Do not derive CLIP/CLAP
            // vectors here as well; the explicit image/audio command merges
            // the optional extension lane in one separate call.
            && q.vector_space.as_deref() != Some("shared")
        {
            if mode_wants_image || !mode_wants_audio {
                let clip_enabled = self.cfg.read().model.clip_enabled;
                let clip = if clip_enabled { self.clip.read().clone() } else { None };
                if let Some(clip) = clip {
                    let key = format!(
                        "clip:{}|{}|query|{}",
                        clip.model_path().display(),
                        clip.dim(),
                        q.text.trim()
                    );
                    match self.cached_aux_query(key, || clip.text_vec(q.text.trim())) {
                        Ok(v) => {
                        q.clip_query = Some(v);
                        // Keep the CLIP query tied to the exact encoder that
                        // produced it. This lets the search engine exclude
                        // persisted PDF page vectors from a previous CLIP
                        // model while retaining standalone media vectors.
                        q.clip_model_key = Some(format!(
                            "clip:{}|{}",
                            clip.model_path().display(),
                            clip.dim()
                        ));
                        }
                        Err(error) => {
                            let _ = self.db.log(
                                "error",
                                &format!("CLIP 图像检索查询编码失败：{error}"),
                            );
                        }
                    }
                }
            }
            if mode_wants_audio || !mode_wants_image {
                let clap_enabled = self.cfg.read().model.clap_enabled;
                let clap = if clap_enabled { self.clap.read().clone() } else { None };
                if let Some(clap) = clap {
                // CLAP 是英文模型：中文查询先映射成英文提示，否则几乎所有
                // 音频都落在同一片「弱相关」余弦区间里，排序等于随机。
                let prompt = crate::ai::clap::prompt_for(q.text.trim());
                    let key = format!(
                        "clap:{}|{}|query|{}",
                        clap.describe(),
                        clap.dim(),
                        prompt
                    );
                    match self.cached_aux_query(key, || clap.text_vec(&prompt)) {
                        Ok(v) => q.audio_query = Some(v),
                        Err(error) => {
                            let _ = self.db.log(
                                "error",
                                &format!("CLAP 音频检索查询编码失败：{error}"),
                            );
                        }
                    }
                }
            }
        }
        let p = self.search_engine();
        let engine = SearchEngine {
            db: &p.db,
            vectors: &p.vectors,
            media_vectors: &p.media,
            clip_vectors: &p.clip,
            audio_vectors: &p.audio,
            cfg: &p.cfg,
        };
        let engine_started = Instant::now();
        let mut hits = engine.search(q);
        let engine_ms = engine_started.elapsed().as_millis();
        if engine_ms >= 100 {
            let _ = self.db.log(
                "perf",
                &format!("index_timing|file=search|stage=engine|ms={engine_ms}"),
            );
        }

        // 工作区实时检索：把「工作区目录里有、索引库里没有」的文件也算进来。
        // 开关由前端传（默认开），只有首页搜索要，其他调用方（相似检索、服务端 API）不需要。
        let workspace_matches_mode = match q.search_mode.as_deref() {
            Some("image" | "audio" | "semantic") => false,
            _ => true,
        };
        // Live workspace extraction is an intentionally expensive fallback:
        // it walks the selected directories and may parse up to hundreds of
        // documents.  Running it after every successful indexed search made
        // the normal search path wait several seconds even when FTS/vector
        // results were already available.  Keep the live path for an
        // explicitly selected directory (that is commonly not indexed yet),
        // and only fall back to it for all/legacy scopes when the indexed
        // lanes produced no hit.  This preserves discovery of unindexed files
        // without blocking the first page of ordinary searches.
        // Live extraction is a fallback for discovering files that are not in
        // the index yet.  Once the indexed lanes produced a result, walking
        // and parsing the selected workspace again only delays the response
        // and can duplicate existing hits.  Keep it disabled for successful
        // searches, including an explicitly selected path; the next empty
        // query still discovers an unindexed path on demand.
        // A live workspace walk is intentionally limited to an explicitly
        // selected path.  Walking every configured root (and especially all
        // drive letters) after an empty query can parse thousands of files on
        // the search command's blocking worker, which made an ordinary search
        // appear to hang for several seconds.  The indexed lanes above are
        // authoritative for `all`/`indexed`; users can still opt into live
        // discovery by selecting a concrete workspace path.
        let should_live_scan = q.workspace.unwrap_or(true)
            && workspace_matches_mode
            && scope == "path"
            && hits.is_empty();
        if should_live_scan {
            let workspace_started = Instant::now();
            let mut ws_cfg = self.cfg.read().workspace.clone();
            if scope == "legacy" && ws_cfg.dirs.is_empty() {
                ws_cfg.dirs = scan.roots.iter()
                    .filter(|root| !scan.disabled_roots.iter().any(|disabled| {
                        crate::util::norm_path(disabled).eq_ignore_ascii_case(&crate::util::norm_path(root))
                    }))
                    .cloned()
                    .collect();
            }
            if scope == "path" {
                if let Some(path) = q.scope_path.as_deref().map(crate::util::norm_path).filter(|p| !p.is_empty()) {
                    ws_cfg.dirs = vec![path];
                }
            } else if scope == "all" {
                // Full-disk scope also covers files that have not been
                // indexed yet. Keep the existing bounded workspace extractor
                // limits so selecting "all" cannot recursively read an
                // unbounded drive in the UI thread.
                ws_cfg.dirs = available_drive_roots();
            }
            if ws_cfg.enabled && !ws_cfg.dirs.is_empty() {
                let extra = self.workspace.search(&self.db, &ws_cfg, q, &hits);
                if !extra.is_empty() {
                    merge_workspace_hits(&mut hits, extra, q);
                }
            }
            let workspace_ms = workspace_started.elapsed().as_millis();
            if workspace_ms >= 100 {
                let _ = self.db.log(
                    "perf",
                    &format!("index_timing|file=search|stage=workspace|ms={workspace_ms}"),
                );
            }
        }
        let mut cache = self.search_cache.lock();
        if cache.len() >= 64 {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (created, _))| *created)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(cache_key, (Instant::now(), hits.clone()));
        hits
    }

    pub fn rebuild_indexer(&self) -> Arc<Indexer> {
        let previous = self.indexer.read().clone();
        let app_cfg = self.cfg.read().clone();
        let cfg = app_cfg.index.clone();
        let embedder = Arc::clone(&self.embedder.read());
        let vecs = Arc::clone(&self.vectors.read());
        let media_vecs = Arc::clone(&self.media_vectors.read());
        let clip_vecs = Arc::clone(&self.clip_vectors.read());
        let audio_vecs = Arc::clone(&self.audio_vectors.read());
        let configured_batch_size = cfg.batch_size;
        let idx = Arc::new(Indexer::new(
            Arc::clone(&self.db),
            vecs,
            media_vecs,
            clip_vecs,
            audio_vecs,
            embedder,
            Arc::clone(&self.media_embedder.read()),
            cfg,
            crate::ocr::OcrConfig::from(&app_cfg.model),
            self.data_dir.join("thumbs"),
        ));
        idx.set_batch_size(crate::ai::effective_batch_size(&app_cfg.model, configured_batch_size));
        idx.set_worker_threads(crate::ai::effective_parallel_threads(&app_cfg.model));
        {
            idx.set_cross_modal(self.clip.read().clone(), self.clap.read().clone());
        }
        // Stop the previous worker before swapping in a new embedder.  The
        // worker owns an Arc<Indexer>, so simply replacing the slot would
        // leave it running with the old model (and potentially its GPU
        // allocation) until process exit.
        // Keep the completed snapshot available to index_stats while the old
        // worker is joined. Holding the slot lock across stop() both stalls UI
        // polling and briefly makes all completed counters disappear.
        if let Some(previous) = previous.as_ref() {
            previous.stop();
            idx.inherit_progress(previous);
        }
        *self.indexer.write() = Some(Arc::clone(&idx));
        idx.start();
        idx
    }

    pub fn indexer(&self) -> Option<Arc<Indexer>> {
        self.indexer.read().clone()
    }

    /// Reopen all persisted vector stores at the configured physical width.
    /// Existing files with another width are left in place for recovery, but
    /// VectorStore intentionally does not load them into the new layout.
    pub fn reopen_vector_stores(&self, dim: usize) -> Result<bool, String> {
        let dim = configured_vector_dim(dim);
        let already = self.vectors.read().dim() == dim
            && self.media_vectors.read().dim() == dim
            && self.clip_vectors.read().dim() == dim
            && self.audio_vectors.read().dim() == dim;
        if already {
            return Ok(false);
        }
        let vectors = VectorStore::open(&self.data_dir.join("vectors"), dim)
            .map_err(|e| format!("文本向量库打开失败: {e}"))?;
        let media = VectorStore::open(&self.data_dir.join("vectors_media"), dim)
            .map_err(|e| format!("媒体向量库打开失败: {e}"))?;
        let clip = VectorStore::open(&self.data_dir.join("vectors_clip"), dim)
            .map_err(|e| format!("图片向量库打开失败: {e}"))?;
        let audio = VectorStore::open(&self.data_dir.join("vectors_audio"), dim)
            .map_err(|e| format!("音频向量库打开失败: {e}"))?;
        let _ = self
            .db
            .restore_video_segment_vectors(&vectors, &media)
            .map_err(|e| format!("video segment vector restore failed: {e}"))?;
        *self.vectors.write() = Arc::new(vectors);
        *self.media_vectors.write() = Arc::new(media);
        *self.clip_vectors.write() = Arc::new(clip);
        *self.audio_vectors.write() = Arc::new(audio);
        Ok(true)
    }

    /// Release the heavy embedding runtime after an indexing pass has reached
    /// a durable idle point.  Search remains available through SQLite/FTS and
    /// the persisted vectors; an explicit semantic search or the next index
    /// request lazily restores the selected model.  The pointer check prevents
    /// an older monitor thread from unloading a model that belongs to a newer
    /// indexing task.
    pub fn release_runtime_model_if_idle(&self, expected: &Arc<Indexer>) {
        if self.model_transition.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _model_change = self.model_change_lock.lock();
        if self.model_transition.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let current = self.indexer.read().clone();
        if current
            .as_ref()
            .map(|value| Arc::ptr_eq(value, expected))
            != Some(true)
            || !expected.is_idle()
        {
            return;
        }
        if self.embedder.read().info().backend == "hash" {
            return;
        }

        // Keep the persisted vector dimension. Reopening a 768-D store as a
        // 256-D hash store would intentionally treat the existing vectors as
        // incompatible and make completed indexes disappear from semantic
        // search until a full rebuild.
        let vector_dim = self.vectors.read().dim();
        let media_dim = self.media_vectors.read().dim();
        let vectors = match VectorStore::open(&self.data_dir.join("vectors"), vector_dim) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.db.log("error", &format!("释放索引模型时打开基础向量库失败：{error}"));
                return;
            }
        };
        let media_vectors = match VectorStore::open(&self.data_dir.join("vectors_media"), media_dim) {
            Ok(value) => value,
            Err(error) => {
                let _ = self.db.log("error", &format!("释放索引模型时打开媒体向量库失败：{error}"));
                return;
            }
        };
        // Temporal video windows are checkpointed in SQLite as well as in the
        // vector-store snapshot.  Reopening the stores while releasing the
        // runtime model can otherwise drop a window that was durably written
        // to the database but had not made it into the last vector snapshot.
        // Restore those rows before swapping the stores so a subsequent
        // semantic/image/audio search remains able to resolve video content
        // and timestamps immediately after indexing completes.
        if let Err(error) = self
            .db
            .restore_video_segment_vectors(&vectors, &media_vectors)
        {
            let _ = self
                .db
                .log("error", &format!("release video vector restore failed: {error}"));
        }
        let old_embedder = std::mem::replace(
            &mut *self.embedder.write(), Arc::new(crate::ai::hash::HashEmbedder::new()),
        );
        let old_media = std::mem::replace(
            &mut *self.media_embedder.write(), Arc::new(crate::ai::hash::HashEmbedder::new()),
        );
        let old_clip = self.clip.write().take();
        let old_clap = self.clap.write().take();
        // TeleOCR may keep a Python/PyTorch process and CUDA allocations
        // alive after the last image batch.  The indexer is durably idle here,
        // so stopping the sidecar cannot interrupt active OCR work. It will
        // be started lazily again for the next OCR/indexing request.
        crate::ocr::shutdown();
        *self.vectors.write() = Arc::new(vectors);
        *self.media_vectors.write() = Arc::new(media_vectors);
        // Replacing the indexer drops the old Arc once its monitor returns,
        // which releases llama/Candle CUDA contexts and their allocations.
        let _ = self.rebuild_indexer();
        drop((old_embedder, old_media, old_clip, old_clap));
        let _ = self.db.log("model", "索引完成，已释放推理模型和 GPU 显存；下次语义检索或建立索引时按需加载");
    }

}

pub fn configured_vector_dim(dim: usize) -> usize {
    match dim {
        256 | 512 | 768 => dim,
        _ => 768,
    }
}

fn available_drive_roots() -> Vec<String> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|letter| format!("{letter}:\\"))
            .filter(|root| std::path::Path::new(root).is_dir())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".into()]
    }
}

/// 把工作区命中并入索引结果：按分数降序，保留原来的分页语义。
///
/// 工作区命中没有真实 id（负数），排序键用分数而不是 id，避免两批结果
/// 混在一起时顺序错乱。offset 已经在 workspace 侧过滤过（只处理首页），
/// 这里只需要把总数截断到 limit。
fn merge_workspace_hits(
    hits: &mut Vec<crate::search::Hit>,
    extra: Vec<crate::search::Hit>,
    q: &crate::search::SearchQuery,
) {
    hits.extend(extra);
    let sort = q.sort.as_deref().unwrap_or("relevance");
    if sort == "relevance" || sort.is_empty() {
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    }
    let limit = q.limit.unwrap_or(50_000).max(1);
    let offset = q.offset.unwrap_or(0);
    if hits.len() > offset + limit {
        hits.truncate(offset + limit);
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    // Do not use AppState::new in tests: its data-directory resolver can
    // deliberately select a populated installation database on the machine.
    pub(crate) fn state() -> (Arc<AppState>, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "deepaisee-search-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let open = |name| Arc::new(VectorStore::open(&root.join(name), 768).unwrap());
        let state = AppState {
            cfg: RwLock::new(AppConfig::default()),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            vectors: RwLock::new(open("vectors")),
            media_vectors: RwLock::new(open("media")),
            clip_vectors: RwLock::new(open("clip")),
            audio_vectors: RwLock::new(open("audio")),
            embedder: RwLock::new(Arc::new(crate::ai::hash::HashEmbedder::new())),
            media_embedder: RwLock::new(Arc::new(crate::ai::hash::HashEmbedder::new())),
            clip: RwLock::new(None),
            clap: RwLock::new(None),
            indexer: RwLock::new(None),
            watcher: Arc::new(FileWatcher::new()),
            server: Arc::new(LanServer::new()),
            data_dir: root.clone(),
            app_dir: root.clone(),
            scan_stats: Arc::new(crate::scan::ScanStatsShared::new()),
            scan_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scan_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            model_transition: std::sync::atomic::AtomicBool::new(false),
            model_change_lock: Mutex::new(()),
            index_settings_dirty: std::sync::atomic::AtomicBool::new(false),
            model_settings_dirty: std::sync::atomic::AtomicBool::new(false),
            workspace: crate::workspace::WorkspaceSearch::new(),
            query_vectors: Mutex::new(HashMap::new()),
            query_vector_inflight: Mutex::new(HashMap::new()),
            search_cache: Mutex::new(HashMap::new()),
        };
        (Arc::new(state), root)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn expired_search_cache_releases_lock_before_refreshing() {
        let (state, root) = test_support::state();
        let mut query = crate::search::SearchQuery {
            text: "活塞环".into(),
            search_mode: Some("fulltext".into()),
            scope: Some("all".into()),
            workspace: Some(false),
            ..Default::default()
        };
        let key = search_cache_key(&query, &state.embedder.read().info().model, 768);
        state.search_cache.lock().insert(key, (Instant::now() - Duration::from_secs(4), vec![]));
        let (send, receive) = std::sync::mpsc::channel();
        let worker_state = Arc::clone(&state);
        std::thread::spawn(move || send.send(worker_state.search(&mut query)).unwrap());
        assert!(receive.recv_timeout(Duration::from_secs(3)).is_ok(), "expired search cache deadlocked");
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_query_lanes_share_one_cold_encoding() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (state, root) = test_support::state();
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let (send, receive) = std::sync::mpsc::channel();
        for _ in 0..4 {
            let state = Arc::clone(&state);
            let calls = Arc::clone(&calls);
            let barrier = Arc::clone(&barrier);
            let send = send.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = state.cached_aux_query("encoder|活塞环".into(), || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    Ok(vec![1.0, 0.0])
                });
                send.send(result).unwrap();
            });
        }
        for _ in 0..4 {
            assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap().unwrap(), vec![1.0, 0.0]);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(state);
        std::fs::remove_dir_all(root).unwrap();
    }
}

