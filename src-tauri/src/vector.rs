//! 向量存储：文件 id -> f32 向量，落盘为二进制并常驻内存。
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub struct VectorStore {
    dim: usize,
    dir: PathBuf,
    data: RwLock<Vec<f32>>,      // slot * dim
    ids: RwLock<Vec<i64>>,       // slot -> file_id（-1 表示空槽）
    index: RwLock<HashMap<i64, usize>>, // file_id -> slot
    generation: AtomicU64,
    saved_generation: AtomicU64,
    save_lock: Mutex<()>,
    encoder: RwLock<Option<EncoderBinding>>,
}

/// Storage width alone does not identify an embedding space. In particular,
/// GGUF and Safetensors adapters can differ in pooling/tokenization even when
/// both return 768 values. Never mix those vectors merely because they fit.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EncoderIdentity {
    pub backend: String,
    pub model: String,
    pub native_dim: usize,
}

impl EncoderIdentity {
    pub fn new(backend: &str, model: &str, native_dim: usize) -> Self {
        let normalized = model.replace('\\', "/");
        Self {
            backend: backend.trim().to_ascii_lowercase(),
            model: if cfg!(windows) {
                normalized.trim_end_matches('/').to_ascii_lowercase()
            } else {
                normalized.trim_end_matches('/').to_string()
            },
            native_dim,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct EncoderBinding {
    identity: EncoderIdentity,
    /// Existing document/video rows use this key, so retain it with the flat
    /// store to filter SQLite recovery after a model change or application restart.
    model_key: String,
    #[serde(default)]
    accept_legacy_rows: bool,
}

pub struct EncoderBindResult {
    pub changed: bool,
    pub legacy: bool,
    pub backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
struct ScoredVector {
    id: i64,
    score: f32,
}

impl PartialEq for ScoredVector {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal && self.id == other.id
    }
}

impl Eq for ScoredVector {}

impl PartialOrd for ScoredVector {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredVector {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the natural order so BinaryHeap::peek() is the smallest
        // retained score, ready to be evicted when a better hit arrives.
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// 读某个向量目录里**已落盘**的维度（meta.json 的 `dim`）。
///
/// CLI 启动时只需要这个数字来判断按什么步长解析 vectors.bin，不应该为此
/// 加载整个 embedding 模型。文件不存在 / 损坏时返回 None。
pub fn saved_dim_of(dir: &Path) -> Option<usize> {
    let s = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let dim = v.get("dim")?.as_u64()? as usize;
    if dim == 0 {
        None
    } else {
        Some(dim)
    }
}

impl VectorStore {
    pub fn open(dir: &Path, dim: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let encoder = match std::fs::read(dir.join("encoder.json")) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid encoder identity: {error}"))
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let store = Self {
            dim,
            dir: dir.to_path_buf(),
            data: RwLock::new(Vec::new()),
            ids: RwLock::new(Vec::new()),
            index: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(0),
            saved_generation: AtomicU64::new(0),
            save_lock: Mutex::new(()),
            encoder: RwLock::new(encoder),
        };
        store.load()?;
        Ok(store)
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Project a backend vector into this store's configured on-disk
    /// dimension. Indexers can persist the exact representation they add so
    /// crash recovery can validate it without re-projecting stale data.
    pub fn fit_vector(&self, vector: &[f32]) -> Vec<f32> {
        if vector.len() == self.dim {
            vector.to_vec()
        } else {
            project_vector(vector, self.dim)
        }
    }

    pub fn len(&self) -> usize {
        self.index.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn encoder_model_key(&self) -> Option<String> {
        self.encoder.read().as_ref().map(|binding| binding.model_key.clone())
    }

    pub fn accepts_unmarked_vectors(&self) -> bool {
        self.encoder.read().as_ref().map_or(true, |binding| binding.accept_legacy_rows)
    }

    /// Bind a store to its encoder without deleting the previous model's
    /// vectors. Call only after the old indexing worker has checkpointed and
    /// stopped. Unmarked legacy stores are adopted once for compatibility;
    /// subsequent encoder changes archive the old payload before clearing it.
    pub fn bind_encoder(
        &self,
        identity: EncoderIdentity,
        model_key: String,
    ) -> std::io::Result<EncoderBindResult> {
        self.save()?;
        let _save_guard = self.save_lock.lock();
        let old = self.encoder.read().clone();
        let changed = old.as_ref().is_some_and(|binding| binding.identity != identity);
        let legacy = old.is_none() && !self.is_empty();
        let mut backup = None;
        if changed {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let archive = self.dir.join("previous-models").join(stamp.to_string());
            std::fs::create_dir_all(&archive)?;
            // A durable copy comes first. If any copy fails, the active
            // snapshot stays intact and callers can report the load failure.
            for name in ["vectors.bin", "ids.bin", "meta.json", "encoder.json"] {
                let source = self.dir.join(name);
                if source.exists() {
                    std::fs::copy(&source, archive.join(name))?;
                }
            }
            for name in ["vectors.bin", "ids.bin", "meta.json"] {
                match std::fs::remove_file(self.dir.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            *self.data.write() = Vec::new();
            *self.ids.write() = Vec::new();
            *self.index.write() = HashMap::new();
            self.generation.fetch_add(1, AtomicOrdering::Release);
            backup = Some(archive);
        }
        let accept_legacy_rows = legacy
            || (!changed && old.as_ref().is_some_and(|binding| binding.accept_legacy_rows));
        let binding = EncoderBinding { identity, model_key, accept_legacy_rows };
        let bytes = serde_json::to_vec(&binding)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = self.dir.join("encoder.json.tmp");
        std::fs::write(&temporary, bytes)?;
        replace(&temporary, &self.dir.join("encoder.json"))?;
        *self.encoder.write() = Some(binding);
        Ok(EncoderBindResult { changed, legacy, backup })
    }

    fn vec_path(&self) -> PathBuf {
        self.dir.join("vectors.bin")
    }
    fn id_path(&self) -> PathBuf {
        self.dir.join("ids.bin")
    }
    fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    /// 读维度元数据。老版本没有 meta.json 时返回 None。
    fn saved_dim(&self) -> Option<usize> {
        let s = std::fs::read_to_string(self.meta_path()).ok()?;
        let v: serde_json::Value = serde_json::from_str(&s).ok()?;
        v.get("dim")?.as_u64().map(|d| d as usize)
    }

    fn write_meta(&self) -> std::io::Result<()> {
        let meta = serde_json::json!({ "dim": self.dim, "count": self.len() });
        std::fs::write(self.meta_path(), meta.to_string())
    }

    fn load(&self) -> std::io::Result<()> {
        let vp = self.vec_path();
        let ip = self.id_path();
        if !vp.exists() || !ip.exists() {
            return Ok(());
        }
        // 维度不一致说明换过 embedding 后端（如 hash 256 → llama 512），
        // 旧向量全部作废，直接清空重建，否则数据会按错误步长错位。
        if let Some(d) = self.saved_dim() {
            if d != self.dim {
                // 必须留下痕迹：这条静默 return 会让「向量库一直是空的」变得
                // 完全不可诊断（用户只看到语义检索永远 0 条、GPU 从不吃显存）。
                eprintln!(
                    "[vector] {} 的维度是 {d}，当前后端需要 {}，旧向量作废并按新维度重建",
                    self.dir.display(),
                    self.dim
                );
                return Ok(());
            }
        }
        let mut buf = Vec::new();
        std::fs::File::open(&vp)?.read_to_end(&mut buf)?;
        let data: Vec<f32> = buf
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut ibuf = Vec::new();
        std::fs::File::open(&ip)?.read_to_end(&mut ibuf)?;
        let ids: Vec<i64> = ibuf
            .chunks_exact(8)
            .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect();
        if self.dim > 0 && data.len() != ids.len() * self.dim {
            // 文件损坏或维度记录缺失：保守起见清空，避免越界。
            // 同样必须留痕，否则向量直接消失且没有任何线索。
            eprintln!(
                "[vector] {} 数据长度 {} 与 ids {} × dim {} 不匹配，按损坏处理并清空",
                self.dir.display(),
                data.len(),
                ids.len(),
                self.dim
            );
            return Ok(());
        }
        let slots = ids.len();
        let mut index = HashMap::with_capacity(slots);
        for (slot, &fid) in ids.iter().enumerate() {
            // `-1` is the tombstone used by remove/remove_matching. Other
            // negative IDs are valid persistent keys: video segments and
            // document page vectors live in negative namespaces.
            if fid != -1 {
                index.insert(fid, slot);
            }
        }
        *self.data.write() = data;
        *self.ids.write() = ids;
        *self.index.write() = index;
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let _save_guard = self.save_lock.lock();
        let generation = self.generation.load(AtomicOrdering::Acquire);
        if generation == self.saved_generation.load(AtomicOrdering::Acquire) {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        // 原子替换：先写临时文件再 rename（Windows 上 rename 覆盖需要先删）
        let tmp = self.dir.join("vectors.bin.tmp");
        let data = self.data.read();
        let ids = self.ids.read();
        {
            let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
            let mut bytes = Vec::with_capacity(16 * 1024 * 4);
            for chunk in data.chunks(16 * 1024) {
                bytes.clear();
                for v in chunk {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                w.write_all(&bytes)?;
            }
            w.flush()?;
        }
        let tmp_id = self.dir.join("ids.bin.tmp");
        {
            let mut w = BufWriter::new(std::fs::File::create(&tmp_id)?);
            let mut bytes = Vec::with_capacity(8 * 1024 * 8);
            for chunk in ids.chunks(8 * 1024) {
                bytes.clear();
                for v in chunk {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                w.write_all(&bytes)?;
            }
            w.flush()?;
        }
        replace(&tmp, &self.vec_path())?;
        replace(&tmp_id, &self.id_path())?;
        drop(ids);
        drop(data);
        self.write_meta()?;
        self.saved_generation.store(generation, AtomicOrdering::Release);
        Ok(())
    }

    pub fn add(&self, file_id: i64, vec: &[f32]) {
        // Different embedding backends expose different native dimensions
        // (for example Gemma=768, CLIP=512 and the hash fallback=256).  The
        // configured store dimension is the on-disk contract, so project at
        // the boundary instead of allowing a mismatched slice to corrupt the
        // slot layout.  The same deterministic mapping is applied to queries
        // in `search`.
        let projected;
        let vec = if vec.len() == self.dim {
            vec
        } else {
            projected = project_vector(vec, self.dim);
            &projected
        };
        let mut index = self.index.write();
        let mut data = self.data.write();
        let mut ids = self.ids.write();
        if let Some(&slot) = index.get(&file_id) {
            data[slot * self.dim..(slot + 1) * self.dim].copy_from_slice(vec);
            self.generation.fetch_add(1, AtomicOrdering::Release);
            return;
        }
        let slot = ids.len();
        ids.push(file_id);
        data.extend_from_slice(vec);
        index.insert(file_id, slot);
        self.generation.fetch_add(1, AtomicOrdering::Release);
    }

    pub fn has(&self, file_id: i64) -> bool {
        self.index.read().contains_key(&file_id)
    }

    /// 取出一批 id 的向量（缺失的跳过），按传入顺序返回。
    ///
    /// 为什么不是逐个 `get()`：每次 get 都要拿两把锁并复制一份 Vec，
    /// 语义星图一次要取几百条，逐个调用等于反复抢锁 + 几百次堆分配。
    /// 这里一次读锁内把整批复制进一块连续内存。
    pub fn get_many(&self, ids: &[i64]) -> Vec<(i64, Vec<f32>)> {
        let index = self.index.read();
        let data = self.data.read();
        let mut out = Vec::with_capacity(ids.len());
        for &fid in ids {
            if let Some(&slot) = index.get(&fid) {
                let base = slot * self.dim;
                out.push((fid, data[base..base + self.dim].to_vec()));
            }
        }
        out
    }

    /// 全部已落盘向量的 id（顺序不保证）。
    ///
    /// 语义星图需要"先决定取样哪些文件，再取它们的向量"：取样要结合目录、
    /// 大小等元数据，光看向量本身做不了分层抽样。
    pub fn all_ids(&self) -> Vec<i64> {
        self.index.read().keys().copied().collect()
    }

    pub fn get(&self, file_id: i64) -> Option<Vec<f32>> {
        let index = self.index.read();
        let slot = *index.get(&file_id)?;
        let data = self.data.read();
        Some(data[slot * self.dim..(slot + 1) * self.dim].to_vec())
    }

    pub fn remove(&self, file_id: i64) {
        let mut index = self.index.write();
        if let Some(slot) = index.remove(&file_id) {
            let mut ids = self.ids.write();
            ids[slot] = -1;
            self.generation.fetch_add(1, AtomicOrdering::Release);
        }
    }

    /// Remove all logical keys matching a predicate in one write-lock pass.
    /// This is used to replace temporal video segments when a file changes.
    pub fn remove_matching<F>(&self, predicate: F)
    where
        F: Fn(i64) -> bool,
    {
        let mut index = self.index.write();
        let mut ids = self.ids.write();
        let mut changed = false;
        for id in ids.iter_mut() {
            if *id == -1 || !predicate(*id) {
                continue;
            }
            index.remove(id);
            *id = -1;
            changed = true;
        }
        if changed {
            self.generation.fetch_add(1, AtomicOrdering::Release);
        }
    }

    /// 余弦相似检索（向量均已归一化，直接点积）。
    pub fn search(&self, query: &[f32], top_k: usize, allowed: Option<&std::collections::HashSet<i64>>) -> Vec<(i64, f32)> {
        if top_k == 0 || query.is_empty() || self.dim == 0 {
            return Vec::new();
        }
        let projected;
        let query = if query.len() == self.dim {
            query
        } else {
            projected = project_vector(query, self.dim);
            &projected
        };
        let index = self.index.read();
        let data = self.data.read();
        let ids = self.ids.read();
        // Keep only the requested top-k candidates. For larger stores, split
        // the dot-product scan across the CPU pool and merge small local
        // heaps. This preserves exact ranking while avoiding a serial
        // 50k x 768 loop on every semantic query.
        let push = |top: &mut BinaryHeap<ScoredVector>, candidate: ScoredVector| {
            if top.len() < top_k {
                top.push(candidate);
            } else if top.peek().is_some_and(|smallest| candidate < *smallest) {
                top.pop();
                top.push(candidate);
            }
        };
        // When a caller already narrowed the search to an allow-list (for
        // example video-only or CLIP-only candidates), iterate that list
        // directly.  Scanning the whole store and checking membership makes
        // a small filtered query pay the cost of the entire vector database,
        // and shared-video searches used to do this twice.
        let top = if let Some(allowed_ids) = allowed {
            if allowed_ids.is_empty() {
                return Vec::new();
            }
            if allowed_ids.len() >= 4096 {
                allowed_ids
                    .par_iter()
                    .fold(
                        || BinaryHeap::with_capacity(top_k.min(256)),
                        |mut local, fid| {
                            let Some(&slot) = index.get(fid) else {
                                return local;
                            };
                            let base = slot * self.dim;
                            let mut dot = 0.0f32;
                            for i in 0..self.dim {
                                dot += data[base + i] * query[i];
                            }
                            push(&mut local, ScoredVector { id: *fid, score: dot });
                            local
                        },
                    )
                    .reduce(
                        || BinaryHeap::new(),
                        |mut left, right| {
                            for candidate in right {
                                push(&mut left, candidate);
                            }
                            left
                        },
                    )
            } else {
                let mut local = BinaryHeap::with_capacity(top_k.min(allowed_ids.len()));
                for &fid in allowed_ids {
                    let Some(&slot) = index.get(&fid) else {
                        continue;
                    };
                    let base = slot * self.dim;
                    let mut dot = 0.0f32;
                    for i in 0..self.dim {
                        dot += data[base + i] * query[i];
                    }
                    push(&mut local, ScoredVector { id: fid, score: dot });
                }
                local
            }
        } else if index.len() >= 4096 {
            // Scan the dense slot arrays instead of the HashMap.  The map
            // iteration visits slots in a random order and causes a cache
            // miss for nearly every 768-float vector.  The persisted vectors
            // are already laid out contiguously, so traversing them by slot
            // lets Rayon and the CPU prefetcher stream through the data.
            data
                .par_chunks(self.dim)
                .enumerate()
                .fold(
                    || BinaryHeap::with_capacity(top_k.min(256)),
                    |mut local, (slot, values)| {
                        let Some(&fid) = ids.get(slot) else {
                            return local;
                        };
                        if fid == -1 {
                            return local;
                        }
                        let mut dot = 0.0f32;
                        for i in 0..self.dim {
                            dot += values[i] * query[i];
                        }
                        push(&mut local, ScoredVector { id: fid, score: dot });
                        local
                    },
                )
                .reduce(
                    || BinaryHeap::new(),
                    |mut left, right| {
                        for candidate in right {
                            push(&mut left, candidate);
                        }
                        left
                    },
                )
        } else {
            let mut local = BinaryHeap::with_capacity(top_k.min(index.len()));
            for (&fid, &slot) in index.iter() {
                let base = slot * self.dim;
                let mut dot = 0.0f32;
                for i in 0..self.dim {
                    dot += data[base + i] * query[i];
                }
                push(&mut local, ScoredVector { id: fid, score: dot });
            }
            local
        };
        let mut out: Vec<_> = top.into_iter().collect();
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.id.cmp(&b.id))
        });
        out.into_iter().map(|candidate| (candidate.id, candidate.score)).collect()
    }

    /// Search the complete store and a high-priority allow-list in one dot
    /// product pass. Shared multimodal stores contain ordinary text/page
    /// vectors as well as temporal video windows. Searching the store and
    /// then searching all video IDs again doubled CPU work for every semantic
    /// query. Keeping both heaps while each vector is loaded preserves the
    /// recall of that second pass without the duplicate scan.
    pub fn search_with_priority_allowlist(
        &self,
        query: &[f32],
        top_k: usize,
        priority_ids: &HashSet<i64>,
    ) -> Vec<(i64, f32)> {
        if top_k == 0 || query.is_empty() || self.dim == 0 || priority_ids.is_empty() {
            return self.search(query, top_k, None);
        }
        let projected;
        let query = if query.len() == self.dim {
            query
        } else {
            projected = project_vector(query, self.dim);
            &projected
        };
        let index = self.index.read();
        let data = self.data.read();
        let ids = self.ids.read();
        let push = |top: &mut BinaryHeap<ScoredVector>, candidate: ScoredVector| {
            if top.len() < top_k {
                top.push(candidate);
            } else if top.peek().is_some_and(|smallest| candidate < *smallest) {
                top.pop();
                top.push(candidate);
            }
        };
        let merge = |mut left: (BinaryHeap<ScoredVector>, BinaryHeap<ScoredVector>),
                     right: (BinaryHeap<ScoredVector>, BinaryHeap<ScoredVector>)| {
            for candidate in right.0 {
                push(&mut left.0, candidate);
            }
            for candidate in right.1 {
                push(&mut left.1, candidate);
            }
            left
        };
        let heaps = if index.len() >= 4096 {
            // Keep this pass slot-aligned with `search()`.  The old HashMap
            // walk visited vector rows in a random order, which was
            // especially costly for the video-priority search because every
            // semantic query used this path.  Streaming contiguous rows lets
            // Rayon and the CPU prefetcher keep the 768-float dot products
            // fed while preserving the exact top-k result.
            data
                .par_chunks(self.dim)
                .enumerate()
                .fold(
                    || (BinaryHeap::with_capacity(top_k.min(256)), BinaryHeap::with_capacity(top_k.min(256))),
                    |mut local, (slot, values)| {
                        let Some(&fid) = ids.get(slot) else {
                            return local;
                        };
                        if fid == -1 {
                            return local;
                        }
                        let mut dot = 0.0f32;
                        for i in 0..self.dim {
                            dot += values[i] * query[i];
                        }
                        let candidate = ScoredVector { id: fid, score: dot };
                        push(&mut local.0, candidate);
                        if priority_ids.contains(&fid) {
                            push(&mut local.1, candidate);
                        }
                        local
                    },
                )
                .reduce(
                    || (BinaryHeap::new(), BinaryHeap::new()),
                    merge,
                )
        } else {
            let mut heaps = (BinaryHeap::with_capacity(top_k.min(index.len())), BinaryHeap::with_capacity(top_k.min(priority_ids.len())));
            for (slot, &fid) in ids.iter().enumerate() {
                if fid == -1 {
                    continue;
                }
                let base = slot * self.dim;
                let mut dot = 0.0f32;
                for i in 0..self.dim {
                    dot += data[base + i] * query[i];
                }
                let candidate = ScoredVector { id: fid, score: dot };
                push(&mut heaps.0, candidate);
                if priority_ids.contains(&fid) {
                    push(&mut heaps.1, candidate);
                }
            }
            heaps
        };
        let mut out: Vec<ScoredVector> = heaps.0.into_iter().chain(heaps.1).collect();
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.id.cmp(&b.id))
        });
        out.dedup_by_key(|candidate| candidate.id);
        out.into_iter().map(|candidate| (candidate.id, candidate.score)).collect()
    }

    pub fn compact(&self) -> std::io::Result<()> {
        let mut index = self.index.write();
        let mut ids = self.ids.write();
        let mut data = self.data.write();
        let mut new_ids = Vec::with_capacity(index.len());
        let mut new_data = Vec::with_capacity(index.len() * self.dim);
        let mut new_index = HashMap::with_capacity(index.len());
        for (slot, &fid) in ids.iter().enumerate() {
            // Preserve all live namespaces, including negative video/page
            // vectors. Only -1 is a tombstone.
            if fid != -1 {
                new_index.insert(fid, new_ids.len());
                new_ids.push(fid);
                new_data.extend_from_slice(&data[slot * self.dim..(slot + 1) * self.dim]);
            }
        }
        *ids = new_ids;
        *data = new_data;
        *index = new_index;
        drop(index);
        drop(ids);
        drop(data);
        // Compaction changes the persisted slot layout even when no logical
        // vector was added or removed since the last save.
        self.generation.fetch_add(1, AtomicOrdering::Release);
        self.save()
    }

    /// Compact only after enough tombstones accumulate to pay for a rewrite.
    /// This is intended for index checkpoints, never for each page mutation.
    pub fn compact_if_needed(&self) -> std::io::Result<bool> {
        let ids = self.ids.read();
        let total = ids.len();
        let tombstones = ids.iter().filter(|&&id| id == -1).count();
        drop(ids);
        if total >= 512 && tombstones >= 128 && tombstones * 4 >= total {
            self.compact()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn clear(&self) -> std::io::Result<()> {
        // Serialize a clear with save(). Without the save lock, a checkpoint
        // that is already writing a snapshot can recreate the files after we
        // remove them, leaving an apparently empty in-memory store backed by
        // stale vectors on the next startup.
        let _save_guard = self.save_lock.lock();
        *self.data.write() = Vec::new();
        *self.ids.write() = Vec::new();
        *self.index.write() = HashMap::new();
        // `clear()` is a durable state transition. Remove metadata as well as
        // the two payload files; otherwise a later open can observe an old
        // dimension/count even though the payload was cleared.
        let _ = std::fs::remove_file(self.vec_path());
        let _ = std::fs::remove_file(self.id_path());
        let _ = std::fs::remove_file(self.meta_path());
        // Advance the generation so a subsequent add/save cannot be skipped
        // because saved_generation still equals the pre-clear generation.
        self.generation.fetch_add(1, AtomicOrdering::Release);
        Ok(())
    }
}

/// Deterministically map a native embedding to the configured storage width.
///
/// This is a storage compatibility mapping, not a claim that unrelated
/// model spaces become semantically interchangeable.  Repeating groups when
/// expanding preserves every source component; folding groups when reducing
/// keeps the result bounded.  The same operation is used for both stored
/// vectors and query vectors, and changing the configured width still causes
/// the caller to rebuild the affected index.
pub fn project_vector(input: &[f32], target: usize) -> Vec<f32> {
    if target == 0 || input.is_empty() {
        return vec![0.0; target];
    }
    if input.len() == target {
        return input.to_vec();
    }
    let mut output = vec![0.0f32; target];
    if target > input.len() {
        for (index, value) in input.iter().copied().enumerate() {
            let start = index * target / input.len();
            let end = ((index + 1) * target / input.len()).max(start + 1).min(target);
            let scale = 1.0f32 / ((end - start) as f32).sqrt();
            for slot in start..end {
                output[slot] = value * scale;
            }
        }
    } else {
        let mut counts = vec![0usize; target];
        for (index, value) in input.iter().copied().enumerate() {
            let slot = index * target / input.len();
            output[slot] += value;
            counts[slot] += 1;
        }
        for (value, count) in output.iter_mut().zip(counts) {
            if count > 1 {
                *value /= (count as f32).sqrt();
            }
        }
    }
    normalize(&mut output);
    output
}

fn replace(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        // Windows 上无法直接 rename 覆盖已有文件
        std::fs::remove_file(dst)?;
    }
    std::fs::rename(tmp, dst)
}

/// 归一化（原地）。
pub fn normalize(v: &mut [f32]) {
    let mut sum = 0.0f64;
    for x in v.iter() {
        sum += (*x as f64) * (*x as f64);
    }
    let n = sum.sqrt();
    if n > 1e-8 {
        let inv = (1.0 / n) as f32;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
    }
    dot
}

#[cfg(test)]
mod tests {
    use super::{Mutex, VectorStore};
    use parking_lot::RwLock;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn fixture() -> VectorStore {
        let ids = vec![1, 2, 3, 4];
        VectorStore {
            dim: 2,
            dir: PathBuf::new(),
            data: RwLock::new(vec![
                1.0, 0.0,
                0.8, 0.6,
                0.8, -0.6,
                0.0, 1.0,
            ]),
            ids: RwLock::new(ids.clone()),
            index: RwLock::new(ids.iter().enumerate().map(|(slot, id)| (*id, slot)).collect::<HashMap<_, _>>()),
            generation: AtomicU64::new(0),
            saved_generation: AtomicU64::new(0),
            save_lock: Mutex::new(()),
            encoder: RwLock::new(None),
        }
    }

    #[test]
    fn search_keeps_exact_top_k_and_stable_ties() {
        let store = fixture();
        assert_eq!(store.search(&[1.0, 0.0], 2, None), vec![(1, 1.0), (2, 0.8)]);
    }

    #[test]
    fn search_respects_allowlist_and_bad_dimensions() {
        let store = fixture();
        let allowed = HashSet::from([2, 3, 4]);
        assert_eq!(
            store.search(&[1.0, 0.0], 3, Some(&allowed)),
            vec![(2, 0.8), (3, 0.8), (4, 0.0)]
        );
        assert!(!store.search(&[1.0], 2, None).is_empty());
        assert!(store.search(&[1.0, 0.0], 0, None).is_empty());
    }

    #[test]
    fn priority_allowlist_matches_union_of_global_and_allowlisted_top_k() {
        let store = fixture();
        let priority = HashSet::from([3, 4]);
        let mut expected = store.search(&[1.0, 0.0], 2, None);
        expected.extend(store.search(&[1.0, 0.0], 2, Some(&priority)));
        expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        expected.dedup_by_key(|item| item.0);
        assert_eq!(
            store.search_with_priority_allowlist(&[1.0, 0.0], 2, &priority),
            expected
        );
    }

    #[test]
    fn project_vector_preserves_width_and_normalizes() {
        let expanded = super::project_vector(&[3.0, 4.0], 6);
        assert_eq!(expanded.len(), 6);
        assert!((expanded.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
        let reduced = super::project_vector(&expanded, 2);
        assert_eq!(reduced.len(), 2);
        assert!((reduced.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn dimension_mismatch_does_not_delete_vectors_needed_by_the_selected_model() {
        let dir = std::env::temp_dir().join(format!(
            "deepdark-vector-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vectors.bin"), [1.0f32, 0.0].into_iter().flat_map(f32::to_le_bytes).collect::<Vec<_>>()).unwrap();
        std::fs::write(dir.join("ids.bin"), 7i64.to_le_bytes()).unwrap();
        std::fs::write(dir.join("meta.json"), r#"{"dim":2,"count":1}"#).unwrap();

        let temporary_hash_view = VectorStore::open(&dir, 256).unwrap();
        assert_eq!(temporary_hash_view.len(), 0);
        assert!(dir.join("vectors.bin").is_file());
        assert!(dir.join("ids.bin").is_file());

        let selected_model_view = VectorStore::open(&dir, 2).unwrap();
        assert_eq!(selected_model_view.get(7), Some(vec![1.0, 0.0]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_width_encoder_switch_archives_old_vectors_and_persists_identity() {
        let dir = std::env::temp_dir().join(format!(
            "deepdark-vector-encoder-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let store = VectorStore::open(&dir, 768).unwrap();
        let gguf = super::EncoderIdentity::new("llama.cpp", "C:/models/embedding.gguf", 768);
        assert!(!store.bind_encoder(gguf.clone(), "embedding.gguf:text|768|280".into()).unwrap().changed);
        let vector = vec![0.25; 768];
        store.add(17, &vector);
        // bind_encoder checkpoints pending data before a possible archive.
        assert!(!store.bind_encoder(gguf, "embedding.gguf:text|768|560".into()).unwrap().changed);
        assert_eq!(store.get(17), Some(vector.clone()));
        let safe = super::EncoderIdentity::new("candle", "C:/models/embedding-safe", 768);
        let switched = store.bind_encoder(safe, "embedding-safe:text|768|280".into()).unwrap();
        assert!(switched.changed);
        assert!(store.is_empty());
        let backup = switched.backup.unwrap();
        let archived = VectorStore::open(&backup, 768).unwrap();
        assert_eq!(archived.get(17), Some(vector));
        let reopened = VectorStore::open(&dir, 768).unwrap();
        assert!(reopened.is_empty());
        assert_eq!(reopened.encoder_model_key().as_deref(), Some("embedding-safe:text|768|280"));
        assert!(!reopened.accepts_unmarked_vectors());
        reopened.clear().unwrap();
        assert_eq!(VectorStore::open(&dir, 768).unwrap().encoder_model_key(), reopened.encoder_model_key());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persisted_negative_media_ids_are_searchable_but_tombstones_are_not() {
        let dir = std::env::temp_dir().join(format!(
            "deepdark-vector-negative-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let vectors: Vec<u8> = [1.0f32, 0.0, 0.0, 1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        std::fs::write(dir.join("vectors.bin"), vectors).unwrap();
        let ids: Vec<u8> = [-1_000_001i64, -1i64]
            .into_iter()
            .flat_map(i64::to_le_bytes)
            .collect();
        std::fs::write(dir.join("ids.bin"), ids).unwrap();
        std::fs::write(dir.join("meta.json"), r#"{"dim":2,"count":2}"#).unwrap();

        let store = VectorStore::open(&dir, 2).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(-1_000_001), Some(vec![1.0, 0.0]));
        assert_eq!(store.get(-1), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checkpoint_compacts_many_tombstones_and_persists_live_negative_ids() {
        let dir = std::env::temp_dir().join(format!(
            "deepdark-vector-compact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = fixture();
        store.dir = dir.clone();
        store.add(-4_000_000_000_000_001_001, &[0.5, 0.5]);
        for id in 1..=640 {
            store.add(id, &[0.5, 0.5]);
        }
        for id in 1..=200 {
            store.remove(id);
        }

        assert!(store.compact_if_needed().unwrap());
        assert_eq!(store.len(), 441);
        assert!(store.has(-4_000_000_000_000_001_001));

        let reopened = VectorStore::open(&dir, 2).unwrap();
        assert_eq!(reopened.len(), 441);
        assert!(reopened.has(-4_000_000_000_000_001_001));
        assert_eq!(
            std::fs::metadata(dir.join("ids.bin")).unwrap().len(),
            441 * std::mem::size_of::<i64>() as u64
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
