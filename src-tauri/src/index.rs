//! 后台索引器：文本抽取 → 缩略图 → 向量化。
use crate::ai::{embed_path, Embedder};
use crate::config::IndexConfig;
use crate::db::Db;
use crate::media;
use crate::vector::VectorStore;
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const VECTOR_CHECKPOINT_ITEMS: usize = 512;
// The UI renders this as one clipped line. Keep enough names to reflect the
// actual single-model batch (rather than reporting only one lane or five
// arbitrary files), while the frontend controls visual overflow.
const ACTIVE_FILE_DISPLAY_LIMIT: usize = 16;
// Keep enough decoded images in flight to overlap disk/CPU work with the
// single model forward pass. The channel is bounded so a large index cannot
// retain an unbounded number of full-resolution rasters.
const IMAGE_PREFETCH_CAPACITY: usize = 32;
/// 单次送进模型的图片上限。
///
/// 原来是 16，而 `ai::effective_batch_size` 在 7 GiB 空闲显存上会给出 32 ——
/// 这里一刀砍半，等于让 GPU 每批只干一半的活就等下一轮 CPU 解码。
/// 提高到 64 之后，真正的上限由 `ai::image_batch_ceiling`（按显存分档）
/// 与 `effective_batch_size` 共同决定，索引时 GPU 不再空转。
const IMAGE_GPU_BATCH_LIMIT: usize = 64;
/// 文本抽取流水线的在途条数。
///
/// 文本路径原来是「整批抽取 → 整批编码」，抽取阶段 GPU 完全空转。
/// 允许几条文本提前抽好排在通道里，GPU 编完一批立刻能拿到下一批，
/// 同时把内存占用限制在几十 MB 量级（每条约 max_text_chars 字符）。
const TEXT_PREFETCH_CAPACITY: usize = 64;

/// 抽取完成、等待送进模型编码的文本。
struct ExtractedText {
    id: i64,
    name: String,
    text: String,
}

fn is_unsupported_backend_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("不支持")
        || lower.contains("unsupported")
        || lower.contains("not support")
        || lower.contains("only supports")
}

/// Media files can be present in the catalog even when the optional FFmpeg
/// decoder is not installed. That is a capability skip, not an embedding
/// failure: the file remains searchable by name and can be re-queued after
/// the decoder is installed. Keep this separate from genuine corrupt-file or
/// model errors so the failure counter remains actionable.
fn is_expected_media_skip_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    is_unsupported_backend_error(error)
        || lower.contains("ffmpeg")
        || lower.contains("audio decode failed")
        || lower.contains("video frame extraction failed")
        || lower.contains("音频解码失败")
        || lower.contains("视频抽帧失败")
        || lower.contains("视频未能解码")
}

/// Split document inputs into small homogeneous buckets before sending them
/// to the model.  `encode_documents` pads a batch to its longest sequence;
/// mixing a 200-character note with a 200k-character document therefore
/// wastes both VRAM and time.  The estimate is deliberately cheap (roughly
/// four UTF-8 characters per tokenizer token) and the backend still performs
/// the authoritative context-length split.
fn text_batch_groups(lengths: &[usize], max_items: usize) -> Vec<Vec<usize>> {
    if lengths.is_empty() {
        return Vec::new();
    }
    let max_items = max_items.clamp(1, 128);
    let mut order: Vec<usize> = (0..lengths.len()).collect();
    order.sort_by_key(|&index| lengths[index]);
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut current_bucket = 0usize;
    let mut current_tokens = 0usize;
    for index in order {
        let estimate = (lengths[index] / 4).max(1);
        let bucket = if estimate <= 512 { 512 } else if estimate <= 2_048 { 2_048 } else { 8_192 };
        // Keep every native batch within the model's shared context window.
        // Without this cap, a large CPU thread budget could assemble many
        // short texts whose combined tokens exceed GGUF's n_batch=8192;
        // llama.cpp then rejects the whole batch and the caller falls back
        // to one-by-one encoding, losing the throughput benefit.
        let token_cap = (max_items * bucket).max(bucket).min(8_192);
        let would_overflow = !current.is_empty()
            && (current.len() >= max_items
                || bucket != current_bucket
                || current_tokens.saturating_add(estimate) > token_cap);
        if would_overflow {
            groups.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        if current.is_empty() {
            current_bucket = bucket;
        }
        current_tokens = current_tokens.saturating_add(estimate);
        current.push(index);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// Work classification is captured when an item enters the queue.  The
/// previous implementation queried SQLite again for every candidate while
/// filling a batch; on a large index that turned scheduling into an O(n^2)
/// stream of database reads.  Keeping this small, copyable tag lets the
/// consumer form homogeneous CPU/GPU batches without touching the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    Pdf,
    Image,
    Text,
    Other,
}

#[derive(Debug, Clone)]
struct QueuedWork {
    id: i64,
    what: String,
    kind: WorkKind,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IndexStats {
    pub text_done: u64,
    pub text_fail: u64,
    pub emb_done: u64,
    pub emb_fail: u64,
    pub thumb_done: u64,
    /// 跨模态向量（CLIP 图片 / CLAP 音频）
    pub cross_done: u64,
    pub cross_fail: u64,
    pub queued: u64,
    /// Current indexing batch size and completed file count.
    pub total: u64,
    pub completed: u64,
    /// Set as soon as the user cancels, and cleared by the next enqueue.
    pub cancelled: bool,
    pub running: bool,
    pub current: Option<String>,
    /// Files currently included in the active model batch. The UI shows this
    /// list so multi-instance work is visible without exposing a counter.
    pub current_items: Vec<String>,
    pub last_error: Option<String>,
    /// Total time spent in the current/resumed indexing task, in seconds.
    /// The UI formats this as MM:SS; keeping it on the backend survives view
    /// changes and does not depend on frontend polling cadence.
    pub elapsed_seconds: u64,
}

pub struct Indexer {
    db: Arc<Db>,
    /// 文本向量空间（llama.cpp GGUF 或 hash 基线）
    vectors: Arc<VectorStore>,
    /// 图片/音频/视频向量空间（hash 特征，维度与文本空间独立）
    media_vectors: Arc<VectorStore>,
    /// 跨模态图片空间（CLIP 512 维），图片与视频关键帧
    clip_vectors: Arc<VectorStore>,
    /// 跨模态音频空间（CLAP 512 维）
    audio_vectors: Arc<VectorStore>,
    /// 跨模态模型（缺模型时为 None，功能静默关闭）
    clip: parking_lot::RwLock<Option<Arc<crate::ai::clip::ClipModel>>>,
    clap: parking_lot::RwLock<Option<Arc<crate::ai::clap::ClapModel>>>,
    embedder: Arc<dyn Embedder>,
    /// 媒体兜底编码器（始终是 hash）
    media_embedder: Arc<dyn Embedder>,
    cfg: IndexConfig,
    batch_size: AtomicU64,
    cpu_pool: parking_lot::RwLock<Arc<rayon::ThreadPool>>,
    progress_what: Mutex<String>,
    /// Scope of the currently requested indexing operation. Keeping this on
    /// the worker prevents progress for one scan root from being reported as
    /// progress for every file in the database.
    progress_roots: Mutex<Option<Vec<String>>>,
    queue: Mutex<VecDeque<QueuedWork>>,
    stats: Mutex<IndexStats>,
    stop: AtomicBool,
    cancel_requested: AtomicBool,
    resume_pending: AtomicBool,
    cancel_checkpointed: AtomicBool,
    busy: AtomicBool,
    active: AtomicU64,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    thumb_dir: PathBuf,
    /// 距上次向量库落盘的插入条数（批量落盘，避免每条都全量写盘）
    dirty: AtomicU64,
    /// Vector states are acknowledged in SQLite only after the corresponding
    /// vector files have been durably replaced. This prevents an interrupted
    /// build from leaving `embed_state=ok` without a persisted vector.
    pending_states: Mutex<Vec<i64>>,
    last_checkpoint_error_log: AtomicU64,
    index_started_at: Mutex<Option<std::time::Instant>>,
    index_elapsed_millis: AtomicU64,
    /// 上次把进度写回 `stats` 的时刻。见 `refresh_progress_throttled`。
    last_progress_refresh: Mutex<Option<std::time::Instant>>,
}

impl Indexer {
    #[inline]
    fn work_kind_of(kind: &str, ext: &str) -> WorkKind {
        if ext.eq_ignore_ascii_case("pdf") {
            WorkKind::Pdf
        } else if kind == "image" {
            WorkKind::Image
        } else if matches!(kind, "text" | "document") {
            WorkKind::Text
        } else {
            WorkKind::Other
        }
    }

    #[inline]
    fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Arc<Db>,
        vectors: Arc<VectorStore>,
        media_vectors: Arc<VectorStore>,
        clip_vectors: Arc<VectorStore>,
        audio_vectors: Arc<VectorStore>,
        embedder: Arc<dyn Embedder>,
        media_embedder: Arc<dyn Embedder>,
        cfg: IndexConfig,
        thumb_dir: PathBuf,
    ) -> Self {
        let configured_batch_size = cfg.batch_size.clamp(1, 128) as u64;
        let cpu_pool = Self::build_cpu_pool(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        );
        let progress_what = db
            .get_meta("index_progress_what")
            .ok()
            .flatten()
            .filter(|value| matches!(value.as_str(), "auto" | "text" | "embed"))
            .unwrap_or_else(|| "auto".into());
        Self {
            db,
            vectors,
            media_vectors,
            clip_vectors,
            audio_vectors,
            clip: parking_lot::RwLock::new(None),
            clap: parking_lot::RwLock::new(None),
            embedder,
            media_embedder,
            cfg,
            batch_size: AtomicU64::new(configured_batch_size),
            cpu_pool: parking_lot::RwLock::new(cpu_pool),
            progress_what: Mutex::new(progress_what),
            progress_roots: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            stats: Mutex::new(IndexStats::default()),
            stop: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            resume_pending: AtomicBool::new(false),
            cancel_checkpointed: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            active: AtomicU64::new(0),
            worker: Mutex::new(None),
            thumb_dir,
            dirty: AtomicU64::new(0),
            pending_states: Mutex::new(Vec::new()),
            last_checkpoint_error_log: AtomicU64::new(0),
            index_started_at: Mutex::new(None),
            index_elapsed_millis: AtomicU64::new(0),
            last_progress_refresh: Mutex::new(None),
        }
    }

    /// 挂上跨模态模型（换模型后要重新挂）。
    pub fn set_cross_modal(
        &self,
        clip: Option<Arc<crate::ai::clip::ClipModel>>,
        clap: Option<Arc<crate::ai::clap::ClapModel>>,
    ) {
        *self.clip.write() = clip;
        *self.clap.write() = clap;
    }

    pub fn snapshot(&self) -> IndexStats {
        let mut s = self.stats.lock().clone();
        s.queued = self.queue.lock().len() as u64;
        s.running = self.busy.load(Ordering::Relaxed);
        let live_millis = self
            .index_started_at
            .lock()
            .as_ref()
            .filter(|_| s.running || s.queued > 0)
            .map(|started| started.elapsed().as_millis() as u64)
            .unwrap_or(0);
        s.elapsed_seconds = self
            .index_elapsed_millis
            .load(Ordering::Relaxed)
            .saturating_add(live_millis)
            / 1_000;
        if !s.running && s.queued == 0 {
            let what = self.progress_what.lock().clone();
            if let Ok((total, completed)) = self.progress(&what) {
                s.total = total.max(0) as u64;
                let in_memory = if what == "text" {
                    0
                } else {
                    self.pending_states.lock().len() as i64
                };
                s.completed = completed
                    .saturating_add(in_memory)
                    .max(0)
                    .min(total.max(0)) as u64;
            }
        }
        s
    }

    pub fn set_progress_scope(&self, roots: Option<Vec<String>>) {
        let normalized = roots.map(|items| {
            items
                .into_iter()
                .map(|root| crate::util::norm_path(&root))
                .filter(|root| !root.is_empty())
                .collect::<Vec<_>>()
        }).filter(|items| !items.is_empty());
        *self.progress_roots.lock() = normalized;
    }

    fn progress(&self, what: &str) -> rusqlite::Result<(i64, i64)> {
        let roots = self.progress_roots.lock().clone();
        match roots {
            Some(roots) => self.db.index_progress_in_roots(what, &roots),
            None => self.db.index_progress(what),
        }
    }

    /// 把数据库里的真实进度写回 `stats`，但最多每 `PROGRESS_REFRESH_INTERVAL` 一次。
    ///
    /// `index_progress` 是两条全表 `COUNT(*)`：28 万行的库上各要 30~70 ms，
    /// 1 TB 的库上会涨到秒级。原实现在 `run_loop` 里**每处理完一批**就调一次，
    /// 于是 GPU 刚要开始下一批就被两次全表扫描卡住 —— 这是"索引时 GPU 占用
    /// 只有 10%"的直接元凶之一。
    ///
    /// 运行中的界面进度本来就靠内存计数器，数据库只在空闲/降频时才需要校准。
    fn refresh_progress_throttled(&self, what: &str) {
        self.refresh_progress(what, false);
    }

    /// `force = true` 时跳过节流（队列刚跑空、索引刚停止）。
    fn refresh_progress(&self, what: &str, force: bool) {
        const PROGRESS_REFRESH_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(2);
        let mut last = self.last_progress_refresh.lock();
        let now = std::time::Instant::now();
        if !force {
            if let Some(previous) = *last {
                if now.duration_since(previous) < PROGRESS_REFRESH_INTERVAL {
                    return;
                }
            }
        }
        *last = Some(now);
        drop(last);
        if let Ok((total, completed)) = self.progress(what) {
            let mut stats = self.stats.lock();
            stats.total = total.max(0) as u64;
            let in_memory = if what == "text" {
                0
            } else {
                self.pending_states.lock().len() as i64
            };
            stats.completed = completed
                .saturating_add(in_memory)
                .max(0)
                .min(total.max(0)) as u64;
        }
    }

    fn set_current_items(&self, names: &[String], lanes: usize) {
        if names.is_empty() {
            return;
        }
        // With one model context, every name in this batch is genuinely being
        // prepared/encoded together. Replicated models are retained only for
        // old state compatibility, so use their lane count when present.
        let count = if lanes <= 1 {
            names.len().min(ACTIVE_FILE_DISPLAY_LIMIT)
        } else {
            lanes.clamp(1, ACTIVE_FILE_DISPLAY_LIMIT).min(names.len())
        };
        let active: Vec<String> = (0..count)
            .map(|lane| names[lane * names.len() / count].clone())
            .collect();
        let mut stats = self.stats.lock();
        stats.current = active.first().cloned();
        stats.current_items = active;
    }

    fn clear_current_batch(&self) {
        let mut stats = self.stats.lock();
        stats.current = None;
        stats.current_items.clear();
    }

    pub fn backlog(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn set_batch_size(&self, batch_size: usize) {
        let batch_size = batch_size.clamp(1, 128);
        self.batch_size.store(batch_size as u64, Ordering::Relaxed);
    }

    pub fn set_worker_threads(&self, threads: usize) {
        *self.cpu_pool.write() = Self::build_cpu_pool(threads);
    }

    fn build_cpu_pool(threads: usize) -> Arc<rayon::ThreadPool> {
        let threads = threads.max(1);
        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap_or_else(|_| {
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(1)
                        .build()
                        .expect("single-thread Rayon pool should initialize")
                }),
        )
    }

    pub fn enqueue(&self, ids: Vec<i64>, what: &str) {
        *self.progress_what.lock() = what.to_string();
        let _ = self.db.set_meta("index_progress_what", what);
        let resume = self.resume_pending.swap(false, Ordering::Relaxed);
        self.cancel_checkpointed.store(false, Ordering::Relaxed);
        if !resume {
            self.index_elapsed_millis.store(0, Ordering::Relaxed);
        }
        *self.index_started_at.lock() = Some(std::time::Instant::now());
        let batch_total = ids.len() as u64;
        // Resolve the category once at enqueue time.  The worker can then
        // select a homogeneous batch without issuing a SQLite query for each
        // candidate on every pass through the queue.
        let categories = self.db.file_categories(&ids).unwrap_or_default();
        let queued: Vec<QueuedWork> = ids
            .into_iter()
            .map(|id| {
                let kind = categories
                    .get(&id)
                    .map(|(kind, ext)| Self::work_kind_of(kind, ext))
                    .unwrap_or(WorkKind::Other);
                QueuedWork { id, what: what.to_string(), kind }
            })
            .collect();
        let mut q = self.queue.lock();
        for item in queued {
            q.push_back(item);
        }
        self.active.store(q.len() as u64, Ordering::Relaxed);
        let mut stats = self.stats.lock();
        stats.cancelled = false;
        if resume && stats.total > stats.completed {
            // 取消后只把仍处于 pending 的文件重新入队。保留原批次的
            // completed/total，让 UI 从上次检查点继续显示，而不是回到 0%。
            stats.total = stats.total.max(stats.completed.saturating_add(batch_total));
        } else {
            stats.total = batch_total;
            stats.completed = 0;
            stats.text_done = 0;
            stats.text_fail = 0;
            stats.emb_done = 0;
            stats.emb_fail = 0;
            stats.thumb_done = 0;
            stats.cross_done = 0;
            stats.cross_fail = 0;
        }
        // 入队时把分母校准成数据库里的真实值（例如"这次只排了 5000 条，
        // 但库里还有 4 万条没做"）。用 force 绕过 run_loop 的节流。
        drop(stats);
        self.refresh_progress(what, true);
        let mut stats = self.stats.lock();
        stats.last_error = None;
    }

    fn finish_elapsed(&self) {
        let mut started = self.index_started_at.lock();
        if let Some(value) = started.take() {
            self.index_elapsed_millis.fetch_add(
                value.elapsed().as_millis() as u64,
                Ordering::Relaxed,
            );
        }
    }

    pub fn reset_progress(&self) {
        let mut stats = self.stats.lock();
        stats.total = 0;
        stats.completed = 0;
        stats.text_done = 0;
        stats.text_fail = 0;
        stats.emb_done = 0;
        stats.emb_fail = 0;
        stats.thumb_done = 0;
        stats.cross_done = 0;
        stats.cross_fail = 0;
    }

    /// 把库里所有 pending 的文件入队。
    pub fn enqueue_all_pending(&self, what: &str) -> usize {
        let ids = self.db.pending_ids_active(what, 100_000).unwrap_or_default();
        let n = ids.len();
        self.enqueue(ids, what);
        n
    }

    /// 同上，但最多只排 `limit` 条（启动阶段用，避免积压几十万条时一开机就满载）。
    pub fn enqueue_all_pending_limited(&self, what: &str, limit: usize) -> usize {
        let ids = self.db.pending_ids_active(what, limit).unwrap_or_default();
        let n = ids.len();
        self.enqueue(ids, what);
        n
    }

    /// 启动后台线程（幂等）。
    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let handle = std::thread::spawn(move || this.run_loop());
        *self.worker.lock() = Some(handle);
    }

    fn run_loop(&self) {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                self.finish_elapsed();
                break;
            }
            if self.cancel_requested.load(Ordering::Relaxed) {
                self.active.store(self.queue.lock().len() as u64, Ordering::Relaxed);
                self.clear_current_batch();
                if !self.cancel_checkpointed.swap(true, Ordering::Relaxed) {
                    if let Err(error) = self.checkpoint() {
                        self.log_checkpoint_error("索引取消时保存失败", &error);
                    }
                }
                self.busy.store(false, Ordering::Relaxed);
                self.finish_elapsed();
                self.cancel_requested.store(false, Ordering::Relaxed);
                continue;
            }
            let item = {
                let mut q = self.queue.lock();
                q.pop_front()
            };
            let Some(first) = item else {
                // 队列空了：把攒着的向量落盘，并把真实进度写回去
                self.flush();
                self.finish_elapsed();
                let progress_what = self.progress_what.lock().clone();
                self.refresh_progress(&progress_what, true);
                std::thread::sleep(std::time::Duration::from_millis(150));
                continue;
            };
            self.busy.store(true, Ordering::Relaxed);
            let first_kind = first.kind;
            let mut batch = vec![first];
            let first_pdf = first_kind == WorkKind::Pdf;
            let first_text_like = first_kind == WorkKind::Text;
            // 图片能不能进批量路径，不该取决于主嵌入模型有没有视觉塔。
            // 纯文本 GGUF（没有 mmproj）时图片本来就该走 media_embedder，
            // 但它同样是"批量更快"的活：解码 + 缩略图 + CLIP 都能并行，
            // 而旧的 first_image 判定为 false 时，整批图片会退化成一条
            // 一条 process_one——既吃不满 CPU 池，也让 GPU 上前向之间
            // 全是空隙。真正的 embedder/store 选择挪到 process_image_batch
            // 内部按主模型的模态决定。
            let first_image = first_kind == WorkKind::Image;
            let batchable = first_text_like || first_image || first_pdf;
            let configured_limit = self.batch_size.load(Ordering::Relaxed).clamp(1, 128) as usize;
            // A PDF can contribute several decoded 1024px page images. Keep
            // its flattened image batch bounded so turbo mode cannot retain
            // gigabytes of raster data while still processing multiple files.
            let batch_limit = if first_pdf { configured_limit.min(8) } else { configured_limit };
            if batch_limit > 1
                && batchable
                && matches!(batch[0].what.as_str(), "auto" | "embed" | "text")
            {
                while batch.len() < batch_limit {
                    let next = {
                        let mut q = self.queue.lock();
                        let position = q.iter().position(|candidate| {
                            if candidate.what != batch[0].what {
                                return false;
                            }
                            candidate.kind == first_kind
                        });
                        position.and_then(|position| q.remove(position))
                    };
                    let Some(next) = next else { break };
                    batch.push(next);
                }
            }
            // Keep the public processing methods independent from queue
            // bookkeeping.  This also preserves the original ordering for
            // vectors and progress updates after a batch is assembled.
            let work: Vec<(i64, String)> = batch
                .iter()
                .map(|item| (item.id, item.what.clone()))
                .collect();
            let r = if first_pdf {
                self.process_pdf_batch(&work, &work[0].1)
            } else if first_image {
                self.process_image_batch(&work)
            } else if batch.len() > 1 {
                self.process_text_batch_parallel(&work, &work[0].1)
            } else {
                self.process_one(work[0].0, &work[0].1)
            };
            self.busy.store(false, Ordering::Relaxed);
            self.active.store(self.queue.lock().len() as u64, Ordering::Relaxed);
            self.clear_current_batch();
            if let Err(e) = r {
                if !self.is_cancelled() {
                    let mut s = self.stats.lock();
                    s.last_error = Some(e.clone());
                    drop(s);
                    let _ = self.db.log("error", &e);
                }
            }
            if self.pending_states.lock().len() >= VECTOR_CHECKPOINT_ITEMS
                || self.dirty.load(Ordering::Relaxed) >= VECTOR_CHECKPOINT_ITEMS as u64
                || self.queue.lock().is_empty()
            {
                if let Err(error) = self.checkpoint() {
                    self.stats.lock().last_error = Some(error.clone());
                    self.log_checkpoint_error("索引检查点保存失败", &error);
                }
            }
            let progress_what = self.progress_what.lock().clone();
            self.refresh_progress_throttled(&progress_what);
            // 每处理 200 条记一次心跳，发布版崩溃时能靠它判断「死在哪一步」。
            let done = self.dirty.load(Ordering::Relaxed);
            if done > 0 && done % 200 == 0 {
                let _ = self.db.log("index", &format!("索引进度：已处理 {done} 条"));
            }
        }
    }

    fn process_one(&self, fid: i64, what: &str) -> Result<(), String> {
        let row = self
            .db
            .get_by_id(fid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("文件 {fid} 不在库中"))?;
        if row.path.is_empty() || !Path::new(&row.path).exists() {
            // 文件已经不在磁盘上了。必须标记掉，否则它永远是 pending，
            // 每次启动都会重新排进队列 —— 库里有几万条这种"幽灵记录"时，
            // 程序会一开机就满载空转，用户以为卡死。
            let _ = self.db.mark_missing(&row.path);
            return Ok(());
        }
        self.set_current_items(std::slice::from_ref(&row.name), 1);

        // 1. 文本
        // auto 模式下这条可能是「正文已建、向量没建」的补索引，重新抽一遍纯属浪费。
        let text_pending = row.text_state == "pending" || row.text_state == "fail";
        if (what == "text" || what == "auto") && text_pending {
            // 之前的判定写成 "text" | "doc"，但 ext_kind() 返回的是 "document"，
            // 结果所有 PDF/DOCX/XLSX 都被当成「不需要抽正文」直接 skip，
            // 用户搜正文永远搜不到 —— 全文检索形同虚设。
            let want_text = matches!(row.kind.as_str(), "text" | "document");
            if want_text {
                match crate::textract::extract_text(Path::new(&row.path), self.cfg.max_text_chars) {
                    Some(text) if !text.trim().is_empty() => {
                        let body = crate::util::index_text(&text);
                        let _ = self.db.set_doc(fid, &crate::util::index_text(&row.name), &body, &text);
                        self.stats.lock().text_done += 1;
                    }
                    // 抽取成功但正文为空（扫描版 PDF / 空表格）：标 skip，
                    // 否则它永远 pending，每次启动都重新排一遍。
                    _ => {
                        let _ = self.db.set_state(fid, "text", "skip");
                        // Empty extraction is a deliberate skip, for example a scanned PDF.
                    }
                }
            } else {
                let _ = self.db.set_state(fid, "text", "skip");
            }
        }

        // A text/document item with no extracted body is a valid skip (for
        // example a scanned document or an empty office file).  In the
        // single-item path we used to continue into `embed_path`, which
        // extracted the same empty body again and turned it into a reported
        // embedding failure.  Keep the state as skip and leave the catalog
        // searchable by filename. PDFs are handled below because their page
        // images can still produce a multimodal vector.
        if matches!(row.kind.as_str(), "text" | "document")
            && !row.ext.eq_ignore_ascii_case("pdf")
            && (what == "embed" || what == "auto")
        {
            let has_text = self
                .db
                .get_doc_raw(fid)
                .ok()
                .flatten()
                .is_some_and(|value| !value.trim().is_empty());
            if !has_text {
                let _ = self.db.set_state(fid, "embed", "skip");
                return Ok(());
            }
        }

        // Archives, executables and unknown binary formats have no supported
        //正文 extractor. They are still retained in the file catalog and can
        // be found by name, but should not be reported as failed AI jobs.
        if matches!(row.kind.as_str(), "archive" | "app" | "other")
            && (what == "embed" || what == "auto")
        {
            let _ = self.db.set_state(fid, "embed", "skip");
            return Ok(());
        }

        // 2. 缩略图
        if matches!(row.kind.as_str(), "image" | "video")
            || (row.kind == "document"
                && matches!(row.ext.to_ascii_lowercase().as_str(), "pdf" | "docx"))
        {
            let dest = self.thumb_dir.join(format!("{fid}.jpg"));
            if !dest.exists() || row.thumb_key.is_none() {
                let _ = std::fs::create_dir_all(&self.thumb_dir);
                let made = if matches!(row.kind.as_str(), "image" | "video") {
                    media::make_thumb(Path::new(&row.path), &dest, self.cfg.image_max_side)
                } else {
                    media::make_document_thumb(
                        Path::new(&row.path),
                        &dest,
                        self.cfg.image_max_side,
                    )
                };
                if made {
                    let _ = self.db.set_thumb(fid, &format!("{fid}.jpg"));
                    self.stats.lock().thumb_done += 1;
                }
            }

            // 2.5 视觉标签：给图片/视频打中文关键词（蓝天/草地/雪景/夜景…）。
            //
            // This optional tagger is independent from the shared
            // EmbeddingGemma multimodal vector path and remains useful as a
            // deterministic fallback when no projector is available.
            //
            // 分析缩略图而不是原图：缩略图已经缩放好（几百像素），
            // 又快又稳；视频也能复用抽出来的中间帧。
            if matches!(row.kind.as_str(), "image" | "video") && row.tags.is_none() {
                let src = if dest.exists() {
                    dest.as_path()
                } else {
                    Path::new(&row.path)
                };
                let tags = crate::vision::describe_file(src, &row.kind);
                if !tags.is_empty() {
                    let joined = tags.join(",");
                    let _ = self.db.update_fields(&row.path, &[("tags", joined)]);
                }
            }
        }

        // PDF extraction is shared by the text and vector stages. This avoids
        // reopening the same ZIP/object streams and gives the multimodal
        // encoder exactly the images that were discovered for this item.
        let is_pdf = row.ext.eq_ignore_ascii_case("pdf");
        let (pdf_text, pdf_images) = if is_pdf && (what == "embed" || what == "auto") {
            let text = self
                .db
                .get_doc_raw(fid)
                .ok()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    crate::textract::extract_text(Path::new(&row.path), self.cfg.max_text_chars)
                        .filter(|value| !value.trim().is_empty())
                });
            let images = crate::textract::pdf_images_with_cancel(
                Path::new(&row.path),
                8,
                || self.is_cancelled(),
            );
            (text, images)
        } else {
            (None, Vec::new())
        };

        // 3. 向量
        //
        // GGUF + mmproj and Candle EmbeddingGemma2 can encode supported media
        // into the same 768-dimensional space. Backends without that
        // capability still fall back to the separate media vector store.
        if what == "embed" || what == "auto" {
            let is_media = matches!(row.kind.as_str(), "image" | "audio" | "video");
            let modality = match row.kind.as_str() {
                "image" => "image",
                "audio" => "audio",
                "video" => "video",
                _ => "",
            };
            let primary_supports_media = is_media
                && self.embedder.info().modalities.iter().any(|item| item == modality);
            let (embedder, store) = if is_media && !primary_supports_media {
                (
                    Arc::clone(&self.media_embedder),
                    Arc::clone(&self.media_vectors),
                )
            } else {
                (Arc::clone(&self.embedder), Arc::clone(&self.vectors))
            };
            let is_shared_video = row.kind == "video"
                && primary_supports_media
                && embedder.info().backend == "candle";
            let video_segments = if is_shared_video {
                Some(embedder.embed_video_segments(Path::new(&row.path)))
            } else {
                None
            };
            let embedded = if is_pdf {
                crate::ai::embed_pdf_parts(embedder.as_ref(), pdf_text.as_deref(), &pdf_images)
            } else if is_shared_video {
                Err("video segments handled separately".into())
            } else {
                embed_path(embedder.as_ref(), Path::new(&row.path), &row.kind)
            };
            if let Some(result) = video_segments {
                match result {
                    Ok(segments) if !segments.is_empty() => {
                        store.remove(fid);
                        store.remove_matching(|id| crate::ai::video_segment_source(id) == Some(fid));
                        for (segment, vector) in segments.into_iter().enumerate() {
                            store.add(crate::ai::video_segment_id(fid, segment), &vector);
                        }
                        self.pending_states.lock().push(fid);
                        self.dirty.fetch_add(1, Ordering::Relaxed);
                        self.stats.lock().emb_done += 1;
                    }
                    Ok(_) => {
                        let _ = self.db.set_state(fid, "embed", "skip");
                        let _ = self.db.log(
                            "index",
                            &format!("跳过媒体 {}：没有可用的视频窗口（需要 FFmpeg 或文件没有可解码轨道）", row.name),
                        );
                    }
                    Err(error) => {
                        if is_expected_media_skip_error(&error) {
                            let _ = self.db.set_state(fid, "embed", "skip");
                            let _ = self.db.log("index", &format!("跳过媒体 {}：{error}", row.name));
                        } else {
                            let _ = self.db.set_state(fid, "embed", "fail");
                            self.stats.lock().emb_fail += 1;
                            return Err(format!("{}: {error}", row.name));
                        }
                    }
                }
            } else {
            match embedded {
                Ok(vec) => {
                    store.add(fid, &vec);
                    self.pending_states.lock().push(fid);
                    // 批量落盘：向量库是全量写盘（几百 MB 量级），每条都 save()
                    // 会让硬盘狂转、索引速度掉到个位数。每 64 条或队列空闲时再写。
                    self.dirty.fetch_add(1, Ordering::Relaxed);
                    self.stats.lock().emb_done += 1;
                }
                Err(e) => {
                    // A scan-only PDF with unsupported image filters is a
                    // valid, permanently empty item. Keep it out of retry
                    // queues and report it as skipped rather than as a model
                    // failure on every subsequent indexing pass.
                    if (is_pdf && e.contains("PDF")) || is_expected_media_skip_error(&e) {
                        let _ = self.db.set_state(fid, "embed", "skip");
                        if !is_pdf {
                            let _ = self.db.log("index", &format!("跳过媒体 {}：{e}", row.name));
                        }
                    } else {
                        let _ = self.db.set_state(fid, "embed", "fail");
                        self.stats.lock().emb_fail += 1;
                        return Err(format!("{}: {e}", row.name));
                    }
                }
            }
            }
        }

        // 4. 跨模态（CLIP / CLAP）：把图片和音频编码到「和文字同一空间」，
        //    这样「一句话描述找图片」才成立。hash 向量只描述颜色/能量分布，
        //    跟文字八竿子打不着，不能拿来给文字查询用。
        self.embed_cross_modal(&row, if is_pdf { Some(&pdf_images) } else { None }, None);

        Ok(())
    }

    /// Process a PDF batch with parallel extraction and flattened model batches.
    /// Text and page images are extracted independently, then fused back into
    /// one primary vector per PDF in the same order as the queue.
    fn process_pdf_batch(&self, batch: &[(i64, String)], what: &str) -> Result<(), String> {
        let pdf_started = std::time::Instant::now();
        let _ = self.db.log("index", &format!("PDF batch start: {} files", batch.len()));
        struct Candidate {
            id: i64,
            name: String,
            path: PathBuf,
            cached_text: Option<String>,
            needs_extract: bool,
            text_pending: bool,
            thumb_valid: bool,
        }
        struct Item {
            id: i64,
            name: String,
            text: Option<String>,
            images: Vec<image::DynamicImage>,
        }

        let mut candidates = Vec::with_capacity(batch.len());
        let mut errors = Vec::new();
        for (id, _) in batch {
            if self.is_cancelled() {
                return Err("index cancellation requested".into());
            }
            let row = match self.db.get_by_id(*id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    errors.push(format!("file {id} is missing from the database"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("failed to read file {id}: {error}"));
                    continue;
                }
            };
            if row.path.is_empty() || !Path::new(&row.path).exists() {
                let _ = self.db.mark_missing(&row.path);
                continue;
            }
            let text_pending = row.text_state == "pending" || row.text_state == "fail";
            let cached_text = self.db.get_doc_raw(*id).ok().flatten();
            candidates.push(Candidate {
                id: *id,
                name: row.name,
                path: PathBuf::from(row.path),
                needs_extract: ((what == "text" || what == "auto") && text_pending)
                    || (what != "text" && cached_text.is_none()),
                cached_text,
                text_pending,
                thumb_valid: row.thumb_key.is_some(),
            });
        }

        let names: Vec<String> = candidates.iter().map(|candidate| candidate.name.clone()).collect();
        self.set_current_items(&names, self.embedder.concurrency());
        let max_chars = self.cfg.max_text_chars;
        let pool = self.cpu_pool.read().clone();
        let extract_started = std::time::Instant::now();
        let extracted: Vec<(Option<String>, Vec<image::DynamicImage>)> = pool.install(|| {
            candidates
                .par_iter()
                .map(|candidate| {
                    if self.is_cancelled() {
                        return (None, Vec::new());
                    }
                    let text = if candidate.needs_extract {
                        crate::textract::extract_text(&candidate.path, max_chars)
                    } else {
                        None
                    };
                    let images = crate::textract::pdf_images_with_cancel(
                        &candidate.path,
                        if what == "text" { 1 } else { 8 },
                        || self.is_cancelled(),
                    );
                    (text, images)
                })
                .collect()
        });

        let extracted_images: usize = extracted.iter().map(|(_, images)| images.len()).sum();
        for (candidate, (_, images)) in candidates.iter().zip(&extracted) {
            if let Some(first_page) = images.first() {
                let dest = self.thumb_dir.join(format!("{}.jpg", candidate.id));
                if !dest.is_file() || !candidate.thumb_valid
                    && media::save_document_thumb_image(
                        first_page,
                        &dest,
                        self.cfg.image_max_side,
                    )
                {
                    let _ = self.db.set_thumb(candidate.id, &format!("{}.jpg", candidate.id));
                    self.stats.lock().thumb_done += 1;
                }
            }
        }
        let _ = self.db.log(
            "index",
            &format!(
                "PDF extract finished: {} files, {} images, {:.2}s",
                extracted.len(),
                extracted_images,
                extract_started.elapsed().as_secs_f64()
            ),
        );

        if self.is_cancelled() {
            return Err("index cancellation requested".into());
        }

        if what == "text" {
            for (candidate, (text, _)) in candidates.into_iter().zip(extracted) {
                if self.is_cancelled() {
                    return Err("index cancellation requested".into());
                }
                if !candidate.text_pending {
                    continue;
                }
                match text.filter(|value| !value.trim().is_empty()) {
                    Some(text) => {
                        let body = crate::util::index_text(&text);
                        let _ = self.db.set_doc(
                            candidate.id,
                            &crate::util::index_text(&candidate.name),
                            &body,
                            &text,
                        );
                        self.stats.lock().text_done += 1;
                    }
                    None => {
                        let _ = self.db.set_state(candidate.id, "text", "skip");
                    }
                }
            }
            return if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) };
        }

        let mut items = Vec::with_capacity(candidates.len());
        for (candidate, (extracted_text, images)) in candidates.into_iter().zip(extracted) {
            let text = extracted_text
                .clone()
                .or_else(|| candidate.cached_text.clone())
                .map(|value| value.chars().take(max_chars).collect::<String>())
                .filter(|value| !value.trim().is_empty());

            if what == "auto" && candidate.text_pending {
                if let Some(value) = extracted_text.as_deref().filter(|value| !value.trim().is_empty()) {
                    let body = crate::util::index_text(value);
                    let _ = self.db.set_doc(
                        candidate.id,
                        &crate::util::index_text(&candidate.name),
                        &body,
                        value,
                    );
                    self.stats.lock().text_done += 1;
                } else {
                    let _ = self.db.set_state(candidate.id, "text", "skip");
                    // Empty extraction is a deliberate skip.
                }
            }

            items.push(Item {
                id: candidate.id,
                name: candidate.name,
                text,
                images,
            });
        }

        let parts: Vec<(Option<String>, &[image::DynamicImage])> = items
            .iter()
            .map(|item| (item.text.clone(), item.images.as_slice()))
            .collect();
        if self.is_cancelled() {
            return Err("index cancellation requested".into());
        }
        let mut errors = Vec::new();
        let embed_started = std::time::Instant::now();
        crate::ai::embed_pdf_parts_batch_with(
            self.embedder.as_ref(),
            &parts,
            || self.is_cancelled(),
            |index, vector| {
                if self.is_cancelled() {
                    return;
                }
                let item = &items[index];
                match vector {
                    Ok(vector) => {
                        self.vectors.add(item.id, &vector);
                        self.pending_states.lock().push(item.id);
                        self.dirty.fetch_add(1, Ordering::Relaxed);
                        self.stats.lock().emb_done += 1;
                        if let Ok(Some(row)) = self.db.get_by_id(item.id) {
                            self.embed_cross_modal(&row, Some(&item.images), None);
                        }
                    }
                    Err(error) if error.contains("PDF") => {
                        let _ = self.db.set_state(item.id, "embed", "skip");
                        if let Ok(Some(row)) = self.db.get_by_id(item.id) {
                            self.embed_cross_modal(&row, Some(&item.images), None);
                        }
                    }
                    Err(error) => {
                        if is_unsupported_backend_error(&error) {
                            let _ = self.db.set_state(item.id, "embed", "skip");
                        } else {
                            let _ = self.db.set_state(item.id, "embed", "fail");
                            self.stats.lock().emb_fail += 1;
                            errors.push(format!("{}: {error}", item.name));
                        }
                    }
                }
            },
        )?;
        let _ = self.db.log(
            "index",
            &format!(
                "PDF embedding finished: {} files, {:.2}s",
                parts.len(),
                embed_started.elapsed().as_secs_f64()
            ),
        );
        let _ = self.db.log(
            "index",
            &format!(
                "PDF batch finished: {} files, {:.2}s",
                parts.len(),
                pdf_started.elapsed().as_secs_f64()
            ),
        );
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    /// Cross-modal vectorization. Missing models and unsupported files are skipped.
    #[allow(dead_code)]
    fn process_image_batch_legacy(&self, batch: &[(i64, String)]) -> Result<(), String> {
        struct Item {
            id: i64,
            name: String,
            path: PathBuf,
        }
        let mut items = Vec::with_capacity(batch.len());
        let mut errors = Vec::new();
        for (id, _) in batch {
            let row = match self.db.get_by_id(*id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    errors.push(format!("文件 {id} 不在数据库中"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("读取文件 {id} 失败：{error}"));
                    continue;
                }
            };
            if row.path.is_empty() || !Path::new(&row.path).exists() {
                let _ = self.db.mark_missing(&row.path);
                continue;
            }
            if let Err(error) = self.process_one(*id, "text") {
                errors.push(error);
                continue;
            }
            items.push(Item {
                id: *id,
                name: row.name,
                path: PathBuf::from(row.path),
            });
        }
        if items.is_empty() {
            return Ok(());
        }

        let names: Vec<String> = items.iter().map(|item| item.name.clone()).collect();
        self.set_current_items(&names, self.embedder.concurrency());
        if self.is_cancelled() {
            return Err("index cancellation requested".into());
        }
        let paths: Vec<&Path> = items.iter().map(|item| item.path.as_path()).collect();
        let vectors: Vec<Result<Vec<f32>, String>> = match self.embedder.embed_image_batch(&paths) {
            Ok(vectors) if vectors.len() == items.len() => vectors.into_iter().map(Ok).collect(),
            batch_result => {
                let reason = match batch_result {
                    Ok(vectors) => format!("批量图片向量数量不匹配：{} / {}", vectors.len(), items.len()),
                    Err(error) => error,
                };
                let _ = self.db.log("error", &format!("图片批量编码失败，改为逐个处理：{reason}"));
                let mut fallback = Vec::with_capacity(items.len());
                for item in &items {
                    if self.is_cancelled() {
                        return Err("index cancellation requested".into());
                    }
                    fallback.push(self.embedder.embed_image(&item.path));
                }
                fallback
            }
        };
        if self.is_cancelled() {
            return Err("index cancellation requested".into());
        }
        for (item, vector) in items.into_iter().zip(vectors) {
            match vector {
                Ok(vector) => {
                    self.vectors.add(item.id, &vector);
                    self.pending_states.lock().push(item.id);
                    self.dirty.fetch_add(1, Ordering::Relaxed);
                    self.stats.lock().emb_done += 1;
                }
                Err(error) => {
                    if is_unsupported_backend_error(&error) {
                        let _ = self.db.set_state(item.id, "embed", "skip");
                    } else {
                        let _ = self.db.set_state(item.id, "embed", "fail");
                        self.stats.lock().emb_fail += 1;
                        errors.push(format!("{}: {error}", item.name));
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn process_image_batch(&self, batch: &[(i64, String)]) -> Result<(), String> {
        struct Item {
            row: crate::db::FileRow,
        }
        struct PreparedImage {
            row: crate::db::FileRow,
            image: Result<image::DynamicImage, String>,
            /// CLIP 的 224×224 CHW 像素输入，已在生产者线程里算好。
            ///
            /// 放进生产者是因为这一步（缩放+归一化）单张要 40~80 ms，
            /// 32 条解码线程本来就闲着，让消费者一个人做等于把并行度
            /// 从 32 压回 1。
            clip_pixels: Option<Result<Vec<f32>, String>>,
        }

        let mut items = Vec::with_capacity(batch.len());
        let mut errors = Vec::new();
        for (id, _) in batch {
            if self.is_cancelled() {
                return Err("index cancellation requested".into());
            }
            let row = match self.db.get_by_id(*id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    errors.push(format!("file {id} is missing from database"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("读取文件 {id} 失败：{error}"));
                    continue;
                }
            };
            if row.path.is_empty() || !Path::new(&row.path).exists() {
                let _ = self.db.mark_missing(&row.path);
                continue;
            }
            items.push(Item { row });
        }
        if items.is_empty() {
            return if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) };
        }

        // 谁来编码这批图：主模型有视觉塔就进共享向量空间，否则用媒体
        // 嵌入器（无 mmproj 时是 256 维 hash）。选择写在这里而不是
        // run_loop 里，是因为批量路径本身对两者都成立——区别只是
        // 目标向量库不同。
        let primary_supports_image = self.embedder.info().modalities.iter().any(|m| m == "image");
        let (embedder, store) = if primary_supports_image {
            (Arc::clone(&self.embedder), Arc::clone(&self.vectors))
        } else {
            (Arc::clone(&self.media_embedder), Arc::clone(&self.media_vectors))
        };
        let names: Vec<String> = items.iter().map(|item| item.row.name.clone()).collect();
        self.set_current_items(&names, IMAGE_GPU_BATCH_LIMIT);
        let pool = self.cpu_pool.read().clone();
        let image_max_side = self.cfg.image_max_side;
        let prefetch_capacity = IMAGE_PREFETCH_CAPACITY.min(items.len()).max(1);
        let gpu_batch_limit = self
            .batch_size
            .load(Ordering::Relaxed)
            .clamp(1, IMAGE_GPU_BATCH_LIMIT as u64) as usize;
        let (sender, receiver) = std::sync::mpsc::sync_channel(prefetch_capacity);
        // 生产者线程要在解码之后顺手把 CLIP 的 224×224 像素备好。
        // 这里先在 scope 外取一次，避免每张图都去抢 RwLock。
        let clip_model = self.clip.read().clone();
        let want_clip = clip_model.is_some();

        // A bounded producer keeps decoded images ahead of the one model
        // consumer, while the capacity cap keeps large directories bounded.
        let pipeline_result = std::thread::scope(|scope| {
            let producer = scope.spawn(|| {
                pool.install(|| {
                    items.par_iter().for_each_with(sender, |sender, item| {
                        if self.is_cancelled() {
                            return;
                        }
                        let row = &item.row;
                        if matches!(row.text_state.as_str(), "pending" | "fail") {
                            let _ = self.db.set_state(row.id, "text", "skip");
                        }
                        let image = image::open(&row.path).map_err(|error| {
                            format!("图片读取失败: {}: {error}", row.path)
                        });
                        if let Ok(image) = &image {
                            let thumb_path = self.thumb_dir.join(format!("{}.jpg", row.id));
                            if (!thumb_path.is_file() || row.thumb_key.is_none())
                                && media::save_document_thumb_image(
                                    image,
                                    &thumb_path,
                                    image_max_side,
                                )
                            {
                                let _ = self.db.set_thumb(row.id, &format!("{}.jpg", row.id));
                                self.stats.lock().thumb_done += 1;
                            }
                            if row.tags.is_none() {
                                // Match the old scalar path: visual tags use
                                // the bounded thumbnail rather than the full
                                // source image, avoiding a second large scan.
                                let rgb = image.thumbnail(image_max_side, image_max_side).to_rgb8();
                                let tags = crate::vision::tags_of(
                                    &crate::vision::analyze(&rgb),
                                    &row.kind,
                                );
                                if !tags.is_empty() {
                                    let joined = tags.join(",");
                                    let _ = self.db.update_fields(&row.path, &[("tags", joined)]);
                                }
                            }
                        }
                        // CLIP 的像素准备也留在这里：它是纯 CPU 的
                        // 缩放/归一化，和上面的解码、缩略图、视觉标签
                        // 属于同一份「可以完全并行」的活。留给消费者
                        // 就会退化成单线程流水线。
                        let clip_pixels = if want_clip && image.is_ok() {
                            Some(image.as_ref().map_err(|e| e.clone()).and_then(|img| {
                                crate::ai::clip::prepare_image_pixels(img)
                            }))
                        } else {
                            None
                        };
                        let _ = sender.send(PreparedImage {
                            row: row.clone(),
                            image,
                            clip_pixels,
                        });
                    });
                });
            });

            let mut pipeline_errors = errors;
            let mut cancelled = false;
            loop {
                let first = match receiver.recv() {
                    Ok(item) => item,
                    Err(_) => break,
                };
                if self.is_cancelled() {
                    // Keep draining the bounded channel until the producer
                    // observes cancellation and exits. Dropping the receiver
                    // here would leave a producer blocked in `send`, making
                    // the cancel button appear to hang.
                    cancelled = true;
                    while receiver.recv().is_ok() {}
                    break;
                }
                let mut prepared = vec![first];
                while prepared.len() < gpu_batch_limit {
                    match receiver.recv() {
                        Ok(item) => prepared.push(item),
                        Err(_) => break,
                    }
                }
                if self.is_cancelled() {
                    cancelled = true;
                    while receiver.recv().is_ok() {}
                    break;
                }

                let mut valid = Vec::with_capacity(prepared.len());
                for item in prepared {
                    match item.image {
                        Ok(image) => valid.push((item.row, image, item.clip_pixels)),
                        Err(error) => {
                            let _ = self.db.set_state(item.row.id, "embed", "fail");
                            self.stats.lock().emb_fail += 1;
                            pipeline_errors.push(format!("{}: {error}", item.row.name));
                        }
                    }
                }
                if valid.is_empty() {
                    continue;
                }
                let current_names: Vec<String> =
                    valid.iter().map(|(row, _, _)| row.name.clone()).collect();
                self.set_current_items(&current_names, IMAGE_GPU_BATCH_LIMIT);
                let image_refs: Vec<&image::DynamicImage> =
                    valid.iter().map(|(_, image, _)| image).collect();
                let vectors: Vec<Result<Vec<f32>, String>> =
                    match embedder.embed_image_data_batch(&image_refs) {
                        Ok(vectors) if vectors.len() == valid.len() => {
                            vectors.into_iter().map(Ok).collect()
                        }
                        batch_result => {
                            let reason = match batch_result {
                                Ok(vectors) => format!(
                                    "批量图片向量数量不匹配：{} / {}",
                                    vectors.len(),
                                    valid.len()
                                ),
                                Err(error) => error,
                            };
                            let _ = self.db.log(
                                "error",
                                &format!("图片批量编码失败，改为逐个处理：{reason}"),
                            );
                            valid
                                .iter()
                                .map(|(_, image, _)| embedder.embed_image_data(image))
                                .collect()
                        }
                    };
                if self.is_cancelled() {
                    cancelled = true;
                    while receiver.recv().is_ok() {}
                    break;
                }
                // CLIP 跨模态：整批一次前向。
                //
                // 逐条调 image_data_vec 时，每张图都要重新提交一次
                // 190 MB 权重的推理请求；DirectML 上这部分启动开销比
                // 计算本身还大（实测吞吐被压在 2~3 张/秒）。整批塞进
                // 一次前向，单张的摊销成本降到 1/N。
                //
                // 像素准备（缩放 + 归一化）已经在生产者线程里做完了，
                // 这里只提交前向。之前这段是整个索引链的瓶颈：它让
                // 32 条解码线程干等单线程消费者做 80~100 ms/张的缩放。
                let clip_batch: Vec<Option<Result<Vec<f32>, String>>> = {
                    match clip_model.clone() {
                        Some(clip) => {
                            let mut slots: Vec<Option<Result<Vec<f32>, String>>> =
                                (0..valid.len()).map(|_| None).collect();
                            let mut pending_index: Vec<usize> = Vec::new();
                            let mut pixel_rows: Vec<&Vec<f32>> = Vec::new();
                            for (index, (row, _, clip_pixels)) in valid.iter().enumerate() {
                                if row.ext.eq_ignore_ascii_case("pdf")
                                    || self.clip_vectors.has(row.id)
                                {
                                    continue;
                                }
                                match clip_pixels {
                                    Some(Ok(pixels)) => {
                                        pending_index.push(index);
                                        pixel_rows.push(pixels);
                                    }
                                    Some(Err(error)) => {
                                        slots[index] = Some(Err(error.clone()));
                                    }
                                    None => {}
                                }
                            }
                            if !pixel_rows.is_empty() {
                                match clip.embed_pixels(&pixel_rows) {
                                    Ok(vectors) if vectors.len() == pending_index.len() => {
                                        for (index, vector) in
                                            pending_index.iter().copied().zip(vectors)
                                        {
                                            slots[index] = Some(Ok(vector));
                                        }
                                    }
                                    Ok(vectors) => {
                                        let reason = format!(
                                            "CLIP 批量向量数量不匹配：{} / {}",
                                            vectors.len(),
                                            pending_index.len()
                                        );
                                        for index in pending_index {
                                            slots[index] = Some(Err(reason.clone()));
                                        }
                                    }
                                    Err(error) => {
                                        for index in pending_index {
                                            slots[index] = Some(Err(error.clone()));
                                        }
                                    }
                                }
                            }
                            slots
                        }
                        None => (0..valid.len()).map(|_| None).collect(),
                    }
                };
                for (((row, image, _), vector), clip_slot) in
                    valid.into_iter().zip(vectors).zip(clip_batch)
                {
                    match vector {
                        Ok(vector) => {
                            store.add(row.id, &vector);
                            self.pending_states.lock().push(row.id);
                            self.dirty.fetch_add(1, Ordering::Relaxed);
                            self.stats.lock().emb_done += 1;
                        }
                        Err(error) => {
                            if is_unsupported_backend_error(&error) {
                                let _ = self.db.set_state(row.id, "embed", "skip");
                            } else {
                                let _ = self.db.set_state(row.id, "embed", "fail");
                                self.stats.lock().emb_fail += 1;
                                pipeline_errors.push(format!("{}: {error}", row.name));
                            }
                        }
                    }
                    // 跨模态向量在同一个阶段写入，位置与旧实现一致：
                    // 主向量落库之后。命中批量结果的直接写，没赶上的
                    // （例如本来就已经有向量）走原来的逐条路径做兜底。
                    match clip_slot {
                        Some(Ok(vector)) => {
                            self.clip_vectors.add(row.id, &vector);
                            self.stats.lock().cross_done += 1;
                        }
                        Some(Err(error)) => {
                            self.record_error(format!("CLIP {}: {error}", row.name));
                        }
                        None => self.embed_cross_modal(&row, None, Some(&image)),
                    }
                }
            }
            if producer.join().is_err() {
                pipeline_errors.push("image CPU producer panicked".into());
            }
            if cancelled || self.is_cancelled() {
                Err("index cancellation requested".into())
            } else if pipeline_errors.is_empty() {
                Ok(())
            } else {
                Err(pipeline_errors.join("; "))
            }
        });
        pipeline_result
    }

    #[allow(dead_code)]
    fn process_text_batch(&self, batch: &[(i64, String)], what: &str) -> Result<(), String> {
        struct Item {
            id: i64,
            name: String,
            text: String,
        }

        let mut items = Vec::with_capacity(batch.len());
        let mut errors = Vec::new();
        for (id, _) in batch {
            let row = match self.db.get_by_id(*id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    errors.push(format!("文件 {id} 不在数据库中"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("读取文件 {id} 失败：{error}"));
                    continue;
                }
            };
            if row.path.is_empty() || !Path::new(&row.path).exists() {
                let _ = self.db.mark_missing(&row.path);
                continue;
            }
            self.stats.lock().current = Some(row.name.clone());

            let mut extracted = None;
            let text_pending = row.text_state == "pending" || row.text_state == "fail";
            if what == "auto" && text_pending {
                match crate::textract::extract_text(Path::new(&row.path), self.cfg.max_text_chars) {
                    Some(text) if !text.trim().is_empty() => {
                        let body = crate::util::index_text(&text);
                        let _ = self.db.set_doc(
                            *id,
                            &crate::util::index_text(&row.name),
                            &body,
                            &text,
                        );
                        self.stats.lock().text_done += 1;
                        extracted = Some(text);
                    }
                    _ => {
                        let _ = self.db.set_state(*id, "text", "skip");
                    }
                }
            }

            let text = extracted
                .or_else(|| self.db.get_doc_raw(*id).ok().flatten())
                .unwrap_or_else(|| {
                    crate::textract::extract_text(Path::new(&row.path), self.cfg.max_text_chars)
                        .unwrap_or_default()
                })
                .chars()
                .take(self.cfg.max_text_chars)
                .collect::<String>();
            if text.trim().is_empty() {
                let _ = self.db.set_state(*id, "embed", "skip");
                errors.push(format!("{}: 没有可向量化的正文", row.name));
                continue;
            }
            items.push(Item {
                id: *id,
                name: row.name,
                text,
            });
        }

        if !items.is_empty() {
            let texts: Vec<String> = items.iter().map(|item| item.text.clone()).collect();
            let vectors: Vec<Result<Vec<f32>, String>> = match self.embedder.embed_text_batch(&texts) {
                Ok(vectors) if vectors.len() == items.len() => {
                    vectors.into_iter().map(Ok).collect()
                }
                batch_result => {
                    let batch_error = match batch_result {
                        Ok(vectors) => format!("批量向量数量不匹配：{} / {}", vectors.len(), items.len()),
                        Err(error) => error,
                    };
                    let vectors: Vec<Result<Vec<f32>, String>> = items
                        .iter()
                        .map(|item| self.embedder.embed_text(&item.text, false))
                        .collect();
                    if vectors.iter().all(|vector| vector.is_err()) {
                        errors.push(format!("批量编码失败（{batch_error}）"));
                    }
                    vectors
                }
            };

            for (item, vector) in items.into_iter().zip(vectors) {
                match vector {
                    Ok(vector) => {
                        self.vectors.add(item.id, &vector);
                        self.pending_states.lock().push(item.id);
                        self.dirty.fetch_add(1, Ordering::Relaxed);
                        self.stats.lock().emb_done += 1;
                    }
                    Err(error) => {
                        if is_unsupported_backend_error(&error) {
                            let _ = self.db.set_state(item.id, "embed", "skip");
                        } else {
                            let _ = self.db.set_state(item.id, "embed", "fail");
                            self.stats.lock().emb_fail += 1;
                            errors.push(format!("{}: {error}", item.name));
                        }
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Parallel extraction path used by the indexing worker. Text extraction
    /// is CPU-heavy while Candle is busy only during the later batch encode;
    /// doing it across the Rayon pool keeps all CPU cores useful without
    /// concurrent SQLite/vector writes.
    fn process_text_batch_parallel(&self, batch: &[(i64, String)], what: &str) -> Result<(), String> {
        struct Candidate {
            id: i64,
            name: String,
            path: PathBuf,
            needs_extract: bool,
            cached_text: Option<String>,
        }
        let mut candidates = Vec::with_capacity(batch.len());
        let mut errors = Vec::new();
        let max_chars = self.cfg.max_text_chars;
        for (id, _) in batch {
            if self.is_cancelled() {
                return Err("index cancellation requested".into());
            }
            let row = match self.db.get_by_id(*id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    errors.push(format!("file {id} is missing from the database"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("failed to read file {id}: {error}"));
                    continue;
                }
            };
            if row.path.is_empty() || !Path::new(&row.path).exists() {
                let _ = self.db.mark_missing(&row.path);
                continue;
            }
            self.stats.lock().current = Some(row.name.clone());
            let pending = row.text_state == "pending" || row.text_state == "fail";
            let cached = self.db.get_doc_raw(*id).ok().flatten();
            let needs_extract = (what == "auto" && pending) || cached.is_none();
            candidates.push(Candidate {
                id: *id,
                name: row.name,
                path: PathBuf::from(row.path),
                needs_extract,
                // Keep a previous extraction as a fallback when a parser
                // temporarily fails (for example a malformed PDF font).
                cached_text: cached,
            });
        }

        // 抽取与编码流水线化：原来这里是「整批 CPU 抽取（阻塞 collect）
        // → 整批 GPU 编码」，抽取几十篇文档要几百毫秒到几秒，这段时间
        // GPU 完全空闲（实测 7 GiB 显存的卡利用率长期只有 5~8%）。
        // 改成有界生产者/消费者之后，GPU 编码这一批时后台线程已在抽下一批。
        let pool = self.cpu_pool.read().clone();
        let prefetch_capacity = TEXT_PREFETCH_CAPACITY.min(candidates.len()).max(1);
        let gpu_batch_limit = self.batch_size.load(Ordering::Relaxed).clamp(1, 128) as usize;
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ExtractedText>(prefetch_capacity);

        let pipeline_result = std::thread::scope(|scope| {
            let producer = scope.spawn(|| {
                pool.install(|| {
                    candidates.par_iter().for_each_with(sender, |sender, candidate| {
                        if self.is_cancelled() {
                            return;
                        }
                        let extracted = if candidate.needs_extract {
                            crate::textract::extract_text(&candidate.path, max_chars)
                                .or_else(|| candidate.cached_text.clone())
                        } else {
                            candidate.cached_text.clone()
                        };
                        let text: String = extracted
                            .unwrap_or_default()
                            .chars()
                            .take(max_chars)
                            .collect();
                        if text.trim().is_empty() {
                            if candidate.needs_extract {
                                let _ = self.db.set_state(candidate.id, "text", "skip");
                            }
                            // Empty/scan-only documents are valid files but
                            // carry no text for the model. Keep them out of
                            // retry/error counts; a later scan can requeue
                            // them if contents or backend capability change.
                            let _ = self.db.set_state(candidate.id, "embed", "skip");
                            return;
                        }
                        if candidate.needs_extract {
                            let body = crate::util::index_text(&text);
                            let _ = self.db.set_doc(
                                candidate.id,
                                &crate::util::index_text(&candidate.name),
                                &body,
                                &text,
                            );
                            self.stats.lock().text_done += 1;
                        }
                        let _ = sender.send(ExtractedText {
                            id: candidate.id,
                            name: candidate.name.clone(),
                            text,
                        });
                    });
                });
            });

            let mut pipeline_errors = errors;
            let mut cancelled = false;
            loop {
                let first = match receiver.recv() {
                    Ok(item) => item,
                    Err(_) => break,
                };
                if self.is_cancelled() {
                    // Drain the bounded channel until the producer notices the
                    // cancellation; dropping the receiver early would leave a
                    // producer blocked in send() and make cancel look hung.
                    cancelled = true;
                    while receiver.recv().is_ok() {}
                    break;
                }
                let mut group = vec![first];
                while group.len() < gpu_batch_limit {
                    match receiver.recv() {
                        Ok(item) => group.push(item),
                        Err(_) => break,
                    }
                }
                if self.is_cancelled() {
                    cancelled = true;
                    while receiver.recv().is_ok() {}
                    break;
                }
                // Split by approximate sequence length so short texts are not
                // padded to the longest document in the batch.
                let lengths: Vec<usize> = group.iter().map(|item| item.text.chars().count()).collect();
                let buckets = text_batch_groups(&lengths, gpu_batch_limit);
                let mut vectors: Vec<Option<Result<Vec<f32>, String>>> =
                    (0..group.len()).map(|_| None).collect();
                for bucket in buckets {
                    if self.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                    let current_names: Vec<String> =
                        bucket.iter().map(|&index| group[index].name.clone()).collect();
                    self.set_current_items(&current_names, current_names.len());
                    let input: Vec<String> =
                        bucket.iter().map(|&index| group[index].text.clone()).collect();
                    match self.embedder.embed_text_batch(&input) {
                        Ok(encoded) if encoded.len() == bucket.len() => {
                            for (index, vector) in bucket.into_iter().zip(encoded) {
                                vectors[index] = Some(Ok(vector));
                            }
                        }
                        Ok(encoded) => {
                            // A malformed batch result is recoverable. Fall
                            // back to one-item encoding and only report an
                            // error when the fallback also fails.
                            let returned = encoded.len().min(bucket.len());
                            for (offset, vector) in encoded.into_iter().take(returned).enumerate() {
                                vectors[bucket[offset]] = Some(Ok(vector));
                            }
                            for &index in bucket.iter().skip(returned) {
                                if self.is_cancelled() {
                                    cancelled = true;
                                    break;
                                }
                                self.set_current_items(std::slice::from_ref(&group[index].name), 1);
                                vectors[index] = Some(self.embedder.embed_text(&group[index].text, false));
                            }
                        }
                        Err(batch_error) => {
                            let mut fallback_failed = true;
                            for index in bucket {
                                if self.is_cancelled() {
                                    cancelled = true;
                                    break;
                                }
                                self.set_current_items(std::slice::from_ref(&group[index].name), 1);
                                let result = self.embedder.embed_text(&group[index].text, false);
                                fallback_failed &= result.is_err();
                                vectors[index] = Some(result);
                            }
                            if fallback_failed {
                                pipeline_errors.push(format!("batch embedding failed: {batch_error}"));
                            }
                        }
                    }
                }
                if cancelled {
                    while receiver.recv().is_ok() {}
                    break;
                }
                let vectors: Vec<Result<Vec<f32>, String>> = vectors
                    .into_iter()
                    .map(|value| value.unwrap_or_else(|| Err("missing batch embedding result".into())))
                    .collect();
                for (item, vector) in group.into_iter().zip(vectors) {
                    match vector {
                        Ok(vector) => {
                            self.vectors.add(item.id, &vector);
                            self.pending_states.lock().push(item.id);
                            self.dirty.fetch_add(1, Ordering::Relaxed);
                            self.stats.lock().emb_done += 1;
                        }
                        Err(error) => {
                            if is_unsupported_backend_error(&error) {
                                let _ = self.db.set_state(item.id, "embed", "skip");
                            } else {
                                let _ = self.db.set_state(item.id, "embed", "fail");
                                self.stats.lock().emb_fail += 1;
                                pipeline_errors.push(format!("{}: {error}", item.name));
                            }
                        }
                    }
                }
            }
            let _ = producer.join();
            (pipeline_errors, cancelled)
        });
        let (pipeline_errors, cancelled) = pipeline_result;
        if cancelled {
            return Err("index cancellation requested".into());
        }
        if pipeline_errors.is_empty() {
            Ok(())
        } else {
            Err(pipeline_errors.join("; "))
        }
    }

    /// 跨模态向量（CLIP / CLAP）。
    ///
    /// `cached_image` 是调用方**已经解码好**的图：主向量那一步解过一次，
    /// 这里再 `image::open` 一遍等于把整个文件重新解码。pixiv 上动辄
    /// 2~4 MB 的大图，这一下就是几十上百毫秒——批量索引时它是除了模型
    /// 前向之外最大的单项开销。
    fn embed_cross_modal(
        &self,
        row: &crate::db::FileRow,
        cached_pdf_images: Option<&[image::DynamicImage]>,
        cached_image: Option<&image::DynamicImage>,
    ) {
        let path = Path::new(&row.path);
        let mut dirty = 0u64;
        match row.kind.as_str() {
            "document" if row.ext.eq_ignore_ascii_case("pdf") => {
                let clip = self.clip.read().clone();
                let Some(clip) = clip else { return };
                if self.clip_vectors.has(row.id) {
                    return;
                }
                let owned_images;
                let images = match cached_pdf_images {
                    Some(images) => images,
                    None => {
                        owned_images = crate::textract::pdf_images(path, 8);
                        &owned_images
                    }
                };
                let mut merged = vec![0.0f32; clip.dim()];
                let mut count = 0usize;
                // Four representative pages keep CLIP work bounded. The
                // primary embedding still covers all decoded pages.
                for image in images.iter().take(4) {
                    if self.is_cancelled() {
                        return;
                    }
                    if let Ok(vector) = clip.image_data_vec(image) {
                        if vector.len() != merged.len() {
                            continue;
                        }
                        for (dst, value) in merged.iter_mut().zip(vector) {
                            *dst += value;
                        }
                        count += 1;
                    }
                }
                if count == 0 {
                    // A text-only PDF or an image filter that we do not
                    // decode is a valid cross-modal skip, not a retryable
                    // model error.
                    return;
                }
                for value in &mut merged {
                    *value /= count as f32;
                }
                crate::ai::l2_normalize(&mut merged);
                self.clip_vectors.add(row.id, &merged);
                self.stats.lock().cross_done += 1;
                dirty += 1;
            }
            "image" => {
                let clip = self.clip.read().clone();
                let Some(clip) = clip else { return };
                if self.clip_vectors.has(row.id) {
                    return;
                }
                // 已经解码过就直接用，省掉一次整图读取 + 解码。
                let encoded = match cached_image {
                    Some(image) => clip.image_data_vec(image),
                    None => clip.image_vec(path),
                };
                match encoded {
                    Ok(v) => {
                        self.clip_vectors.add(row.id, &v);
                        self.stats.lock().cross_done += 1;
                        dirty += 1;
                    }
                    Err(e) => {
                        self.record_error(format!("CLIP {}: {e}", row.name));
                    }
                }
            }
            "video" => {
                // 视频：智能抽帧（间隔 + I 帧/场景切换优先 + 指纹去重）后
                // 把多帧 CLIP 向量求平均。旧实现只取中间一帧，片尾字幕、
                // 转场黑场一类的视频经常抽到无意义画面，直接搜不到。
                let clip = self.clip.read().clone();
                if let Some(clip) = clip {
                    if !self.clip_vectors.has(row.id) {
                        let frames = media::sample_video_frames_smart(
                            path,
                            media::FramePlan {
                                interval: self.cfg.video_frame_interval,
                                // 跨模态只需要少量代表帧，帧数太多纯属浪费
                                max_frames: self.cfg.video_max_frames.clamp(1, 12),
                                max_side: 256,
                                dedup_distance: self.cfg.video_dedup_distance,
                            },
                        );
                        if frames.is_empty() {
                            // Cross-modal CLIP is optional. Missing FFmpeg or
                            // a video without a decodable stream is a normal
                            // capability skip and must not inflate failures.
                            let _ = self.db.log(
                                "index",
                                &format!("跳过视频 {} 的 CLIP 抽帧：需要 FFmpeg 或没有可解码画面", row.name),
                            );
                        } else {
                            let mut merged = vec![0.0f32; clip.dim()];
                            let mut frames_used = 0usize;
                            for frame in &frames {
                                if self.is_cancelled() {
                                    break;
                                }
                                if let Ok(v) = clip.image_vec(frame) {
                                    if v.len() == merged.len() {
                                        for (dst, value) in merged.iter_mut().zip(v) {
                                            *dst += value;
                                        }
                                        frames_used += 1;
                                    }
                                }
                                let _ = std::fs::remove_file(frame);
                            }
                            if frames_used > 0 {
                                for value in &mut merged {
                                    *value /= frames_used as f32;
                                }
                                crate::ai::l2_normalize(&mut merged);
                                self.clip_vectors.add(row.id, &merged);
                                self.stats.lock().cross_done += 1;
                                dirty += 1;
                            }
                        }
                    }
                }
                // 音轨语义索引：视频里的说话声/环境声同样能用一句话搜到。
                // 复用 CLAP 的音频向量空间，和纯音频文件放在一起，
                // 「找一段下雨的声音」能同时命中音频和视频。
                if self.cfg.video_index_audio {
                    let clap = self.clap.read().clone();
                    if let Some(clap) = clap {
                        if !self.audio_vectors.has(row.id) {
                            // 无声视频解不出音轨是正常情况，不记错误，
                            // 否则每批索引都会刷一屏「失败」。
                            if let Ok(v) = clap.audio_vec(path) {
                                self.audio_vectors.add(row.id, &v);
                                self.stats.lock().cross_done += 1;
                                dirty += 1;
                            }
                        }
                    }
                }
            }
            "audio" => {
                let clap = self.clap.read().clone();
                let Some(clap) = clap else { return };
                if self.audio_vectors.has(row.id) {
                    return;
                }
                match clap.audio_vec(path) {
                    Ok(v) => {
                        self.audio_vectors.add(row.id, &v);
                        self.stats.lock().cross_done += 1;
                        dirty += 1;
                    }
                    Err(e) => {
                        if is_expected_media_skip_error(&e) {
                            let _ = self.db.log("index", &format!("跳过音频 {} 的 CLAP：{e}", row.name));
                        } else {
                            self.record_error(format!("CLAP {}: {e}", row.name));
                        }
                    }
                }
            }
            _ => {}
        }
        if dirty > 0 {
            self.dirty.fetch_add(dirty, Ordering::Relaxed);
        }
    }

    fn record_error(&self, message: String) {
        {
            let mut stats = self.stats.lock();
            stats.cross_fail += 1;
            stats.last_error = Some(message.clone());
        }
        let _ = self.db.log("error", &message);
    }

    fn checkpoint(&self) -> Result<(), String> {
        self.vectors
            .save()
            .map_err(|error| format!("文本向量保存失败：{error}"))?;
        self.media_vectors
            .save()
            .map_err(|error| format!("媒体向量保存失败：{error}"))?;
        self.clip_vectors
            .save()
            .map_err(|error| format!("CLIP 向量保存失败：{error}"))?;
        self.audio_vectors
            .save()
            .map_err(|error| format!("CLAP 向量保存失败：{error}"))?;

        let ids = std::mem::take(&mut *self.pending_states.lock());
        for id in ids {
            self.db
                .set_state(id, "embed", "ok")
                .map_err(|error| format!("提交文件 {id} 的索引状态失败：{error}"))?;
        }
        self.dirty.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn log_checkpoint_error(&self, context: &str, error: &str) {
        let now = crate::util::now_ts().max(0) as u64;
        let previous = self.last_checkpoint_error_log.load(Ordering::Relaxed);
        if now.saturating_sub(previous) >= 10
            && self
                .last_checkpoint_error_log
                .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let _ = self.db.log("error", &format!("{context}：{error}"));
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let handle = self.worker.lock().take();
        if let Some(handle) = handle {
            if std::thread::current().id() != handle.thread().id() {
                let _ = handle.join();
            }
        }
        if let Err(error) = self.checkpoint() {
            self.log_checkpoint_error("停止索引时保存失败", &error);
        }
    }

    /// Cancel queued work while keeping this indexer alive for the next batch.
    pub fn cancel_pending(&self) {
        self.cancel_requested.store(true, Ordering::Relaxed);
        // Mark the batch as resumable immediately. The worker will perform the
        // durable checkpoint on its next turn, but a new scan may finish before
        // that turn and must still retain the previous completed/total counts.
        self.resume_pending.store(true, Ordering::Relaxed);
        self.cancel_checkpointed.store(false, Ordering::Relaxed);
        self.queue.lock().clear();
        self.active.store(0, Ordering::Relaxed);
        let mut stats = self.stats.lock();
        stats.cancelled = true;
        stats.current = None;
        stats.current_items.clear();
    }

    /// 把缓冲区里的向量立刻写盘（队列空闲时调用）。
    pub fn flush(&self) {
        if self.dirty.load(Ordering::Relaxed) > 0 || !self.pending_states.lock().is_empty() {
            if let Err(error) = self.checkpoint() {
                self.log_checkpoint_error("索引刷新失败", &error);
            }
        }
    }

    /// 等待队列清空（最多 timeout 秒）。
    pub fn wait_idle(&self, timeout_secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if self.queue.lock().is_empty() && !self.busy.load(Ordering::Relaxed) {
                // 再宽限两轮，避免刚出队还没置 busy 的空档
                std::thread::sleep(std::time::Duration::from_millis(200));
                if self.queue.lock().is_empty() && !self.busy.load(Ordering::Relaxed) {
                    return true;
                }
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_expected_media_skip_error, text_batch_groups};

    #[test]
    fn decoder_capability_errors_are_skips_but_model_errors_are_failures() {
        assert!(is_expected_media_skip_error("audio decode failed: ffmpeg not found"));
        assert!(is_expected_media_skip_error("视频抽帧失败（需要 ffmpeg）"));
        assert!(is_expected_media_skip_error("当前后端不支持音频"));
        assert!(!is_expected_media_skip_error("Candle 前向计算失败: out of memory"));
        assert!(!is_expected_media_skip_error("图片读取失败: invalid PNG"));
    }

    #[test]
    fn text_batches_are_bucketed_without_losing_items() {
        let lengths = [80, 9_000, 120, 16_000, 2_000, 240];
        let groups = text_batch_groups(&lengths, 2);
        let mut seen = Vec::new();
        for group in &groups {
            assert!(group.len() <= 2);
            seen.extend(group.iter().copied());
            let first_bucket = (lengths[group[0]] / 4).max(1);
            let first_bucket = if first_bucket <= 512 { 512 } else if first_bucket <= 2_048 { 2_048 } else { 8_192 };
            for &index in group {
                let estimate = (lengths[index] / 4).max(1);
                let bucket = if estimate <= 512 { 512 } else if estimate <= 2_048 { 2_048 } else { 8_192 };
                assert_eq!(bucket, first_bucket);
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..lengths.len()).collect::<Vec<_>>());
    }

    #[test]
    fn text_batch_groups_handle_empty_and_single_item_inputs() {
        assert!(text_batch_groups(&[], 8).is_empty());
        assert_eq!(text_batch_groups(&[10], 0), vec![vec![0]]);
    }
}
