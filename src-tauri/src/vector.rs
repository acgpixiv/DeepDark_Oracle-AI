//! 向量存储：文件 id -> f32 向量，落盘为二进制并常驻内存。
use parking_lot::{Mutex, RwLock};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
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
        let store = Self {
            dim,
            dir: dir.to_path_buf(),
            data: RwLock::new(Vec::new()),
            ids: RwLock::new(Vec::new()),
            index: RwLock::new(HashMap::new()),
            generation: AtomicU64::new(0),
            saved_generation: AtomicU64::new(0),
            save_lock: Mutex::new(()),
        };
        store.load()?;
        Ok(store)
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.index.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
            if fid >= 0 {
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
        debug_assert_eq!(vec.len(), self.dim);
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
        if top_k == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let index = self.index.read();
        let data = self.data.read();
        // Keep only the requested top-k candidates. This avoids allocating and
        // sorting a vector proportional to the entire index for every query.
        let mut top = BinaryHeap::with_capacity(top_k.min(index.len()));
        for (&fid, &slot) in index.iter() {
            if let Some(a) = allowed {
                if !a.contains(&fid) {
                    continue;
                }
            }
            let base = slot * self.dim;
            let mut dot = 0.0f32;
            for i in 0..self.dim {
                dot += data[base + i] * query[i];
            }
            let candidate = ScoredVector { id: fid, score: dot };
            if top.len() < top_k {
                top.push(candidate);
            } else if top.peek().is_some_and(|smallest| candidate < *smallest) {
                top.pop();
                top.push(candidate);
            }
        }
        let mut out: Vec<_> = top.into_iter().collect();
        out.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.id.cmp(&b.id))
        });
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
            if fid >= 0 {
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
        self.save()
    }

    pub fn clear(&self) -> std::io::Result<()> {
        *self.data.write() = Vec::new();
        *self.ids.write() = Vec::new();
        *self.index.write() = HashMap::new();
        let _ = std::fs::remove_file(self.vec_path());
        let _ = std::fs::remove_file(self.id_path());
        Ok(())
    }
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
        assert!(store.search(&[1.0], 2, None).is_empty());
        assert!(store.search(&[1.0, 0.0], 0, None).is_empty());
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
}
