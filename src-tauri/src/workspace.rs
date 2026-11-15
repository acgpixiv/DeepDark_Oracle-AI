//! 工作区实时检索：对尚未入库的文件当场抽取正文参与搜索。
//!
//! 场景：用户在主页临时选一个目录（比如刚下载的资料），不想为它建索引，
//! 但又想立刻搜到里面的内容。这里只处理「工作区里有、索引库里没有」的文件，
//! 已入库的文件仍然走正常的索引检索链路，两批结果按分数合并后统一返回。
use crate::config::WorkspaceConfig;
use crate::db::Db;
use crate::search::{Hit, SearchQuery};
use crate::util;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_EXTRACT_FILES: usize = 160;
const MAX_EXTRACT_CHARS: usize = 8_000_000;
const LISTING_TTL: Duration = Duration::from_secs(4);
const INDEXED_TTL: Duration = Duration::from_secs(10);
const MAX_CACHE_ENTRIES: usize = 5_000;

struct CachedDoc {
    mtime: i64,
    size: u64,
    text: String,
}

#[derive(Default)]
pub struct WorkspaceSearch {
    cache: Mutex<HashMap<String, CachedDoc>>,
    listing: Mutex<Option<(Instant, String, Vec<PathBuf>)>>,
    indexed: Mutex<Option<(Instant, String, HashSet<String>)>>,
}

impl WorkspaceSearch {
    pub fn new() -> Self {
        Self::default()
    }

    /// 合并工作区命中。existing 是索引链路已经返回的结果，用于去重。
    pub fn search(
        &self,
        db: &Db,
        cfg: &WorkspaceConfig,
        q: &SearchQuery,
        existing: &[Hit],
    ) -> Vec<Hit> {
        if !cfg.enabled || cfg.dirs.is_empty() {
            return vec![];
        }
        let text = q.text.trim().to_string();
        if text.chars().count() < 2 {
            return vec![];
        }
        if q.offset.unwrap_or(0) > 0 {
            // 分页时不做现场抽取：成本太高，结果顺序也不稳定。
            return vec![];
        }
        let ql = text.to_lowercase();
        let qchars = text.chars().count() as f32;
        let terms = crate::search::tokenize(&text);
        let indexed = self.indexed_set(db, cfg);
        let files = self.listing(cfg);
        let already: HashSet<String> = existing
            .iter()
            .map(|h| h.path.to_lowercase().replace('/', "\\"))
            .collect();

        let mut out: Vec<Hit> = Vec::new();
        let mut extracted = 0usize;
        let mut chars = 0usize;
        let mut seq: i64 = 0;
        let max_bytes = cfg.max_file_mb.saturating_mul(1024 * 1024);

        for path in files {
            let key = path.to_string_lossy().to_lowercase();
            if indexed.contains(&key) || already.contains(&key) {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() {
                continue;
            }
            let size = meta.len();
            if max_bytes > 0 && size > max_bytes {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let kind = util::ext_kind(&ext);
            if !kind_allowed(q, kind) {
                continue;
            }

            let mut score = name_match(&ql, qchars, &terms, &name);
            let mut reason = name_reason(&ql, &terms, &name);
            let mut snippet: Option<String> = None;

            // 名称没命中、或命中很弱时才抽正文（抽正文是重活）。
            if score < 0.9 && cfg.live_extract && extracted < MAX_EXTRACT_FILES && chars < MAX_EXTRACT_CHARS {
                if let Some(body) = self.body(&path, mtime, size) {
                    extracted += 1;
                    chars += body.chars().count();
                    if let Some(hit) = body_match(&body, &ql, &terms) {
                        let body_score = 0.55 + 0.25 * hit.ratio;
                        if body_score > score {
                            score = body_score;
                            reason = "工作区正文匹配".to_string();
                            snippet = Some(hit.snippet);
                        }
                    }
                }
            }

            // Keep workspace results consistent with indexed search: weak
            // matches below the user-visible 0.40 relevance floor are hidden.
            if score < crate::search::MIN_RESULT_SCORE {
                continue;
            }
            seq += 1;
            out.push(Hit {
                // 负数 id：明确表示「不属于索引库」，前端不会拿它请求缩略图或重命名。
                id: -seq,
                path: util::norm_path(&path.to_string_lossy()),
                dir: path
                    .parent()
                    .map(|d| d.to_string_lossy().to_string())
                    .unwrap_or_default(),
                name,
                ext,
                kind: kind.to_string(),
                kind_label: util::kind_label(kind).to_string(),
                size: size as i64,
                size_label: util::human_size(size as i64),
                mtime,
                mtime_label: util::format_ts(mtime),
                category: None,
                thumb: false,
                thumb_key: None,
                score,
                page: None,
                start_seconds: None,
                end_seconds: None,
                subtitle: None,
                reason,
                snippet,
                tags: Some("工作区".into()),
            });
            if out.len() >= 400 {
                break;
            }
        }
        out
    }
    fn body(&self, path: &Path, mtime: i64, size: u64) -> Option<String> {
        let key = path.to_string_lossy().to_lowercase();
        {
            let cache = self.cache.lock();
            if let Some(doc) = cache.get(&key) {
                if doc.mtime == mtime && doc.size == size {
                    return Some(doc.text.clone());
                }
            }
        }
        let text = crate::textract::extract_text(path, crate::textract::MAX_CHARS_DEFAULT)?;
        if text.trim().is_empty() {
            return None;
        }
        let mut cache = self.cache.lock();
        if cache.len() > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            key,
            CachedDoc {
                mtime,
                size,
                text: text.clone(),
            },
        );
        Some(text)
    }

    fn listing(&self, cfg: &WorkspaceConfig) -> Vec<PathBuf> {
        let signature = cfg.dirs.join("|");
        {
            let guard = self.listing.lock();
            if let Some((ts, cached_signature, files)) = guard.as_ref() {
                if ts.elapsed() < LISTING_TTL && cached_signature == &signature {
                    return files.clone();
                }
            }
        }
        let depth = if cfg.depth == 0 { 1 } else { cfg.depth as usize + 1 };
        let mut files = Vec::new();
        for dir in &cfg.dirs {
            let root = Path::new(dir);
            if !root.is_dir() {
                continue;
            }
            let walker = walkdir::WalkDir::new(root)
                .max_depth(depth)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| !is_hidden_or_noise(e.path()));
            for entry in walker.flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                files.push(entry.into_path());
                if files.len() >= 20_000 {
                    break;
                }
            }
        }
        let mut guard = self.listing.lock();
        *guard = Some((Instant::now(), signature, files.clone()));
        files
    }

    fn indexed_set(&self, db: &Db, cfg: &WorkspaceConfig) -> HashSet<String> {
        let signature = cfg.dirs.join("|");
        {
            let guard = self.indexed.lock();
            if let Some((ts, sig, set)) = guard.as_ref() {
                if ts.elapsed() < INDEXED_TTL && sig == &signature {
                    return set.clone();
                }
            }
        }
        let mut set = HashSet::new();
        for dir in &cfg.dirs {
            if let Ok(paths) = db.paths_under(dir) {
                for p in paths {
                    set.insert(p.to_lowercase().replace('/', "\\"));
                }
            }
        }
        let mut guard = self.indexed.lock();
        *guard = Some((Instant::now(), signature, set.clone()));
        set
    }
}

fn is_hidden_or_noise(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return false;
    }
    let lower = name.to_lowercase();
    lower == "node_modules"
        || lower == ".git"
        || lower == "$recycle.bin"
        || lower == "system volume information"
}

fn kind_allowed(q: &SearchQuery, kind: &str) -> bool {
    q.kinds.is_empty() || q.kinds.iter().any(|k| k == kind)
}

fn name_match(ql: &str, query_chars: f32, terms: &[String], name: &str) -> f32 {
    let nl = name.to_lowercase();
    if nl.as_str() == ql {
        return 1.0;
    }
    if nl.starts_with(ql) {
        return 0.92;
    }
    if nl.contains(ql) {
        let cover = query_chars / nl.chars().count().max(1) as f32;
        return 0.72 + 0.26 * cover.min(1.0);
    }
    if terms.len() > 1 {
        let hit = terms.iter().filter(|t| nl.contains(t.as_str())).count();
        if hit > 0 {
            return 0.55 + 0.2 * (hit as f32 / terms.len() as f32);
        }
    }
    0.0
}

fn name_reason(ql: &str, terms: &[String], name: &str) -> String {
    let nl = name.to_lowercase();
    if nl.as_str() == ql {
        "名称精确匹配".into()
    } else if nl.starts_with(ql) {
        "名称前缀匹配".into()
    } else if nl.contains(ql) {
        "名称包含".into()
    } else if terms.len() > 1 && terms.iter().any(|t| nl.contains(t.as_str())) {
        "名称多词命中".into()
    } else {
        "工作区匹配".into()
    }
}

struct BodyHit {
    ratio: f32,
    snippet: String,
}

fn body_match(body: &str, ql: &str, terms: &[String]) -> Option<BodyHit> {
    let lower = body.to_lowercase();
    if let Some(pos) = lower.find(ql) {
        return Some(BodyHit {
            ratio: 1.0,
            snippet: snippet_at(body, &lower, pos, ql.chars().count()),
        });
    }
    let mut best: Option<(usize, usize)> = None;
    let mut hits = 0usize;
    let valid = terms.iter().filter(|t| t.chars().count() >= 2).count().max(1);
    for term in terms {
        if term.chars().count() < 2 {
            continue;
        }
        if let Some(pos) = lower.find(term.as_str()) {
            hits += 1;
            let len = term.chars().count();
            match best {
                Some((_, blen)) if blen >= len => {}
                _ => best = Some((pos, len)),
            }
        }
    }
    let (pos, len) = best?;
    Some(BodyHit {
        ratio: hits as f32 / valid as f32,
        snippet: snippet_at(body, &lower, pos, len),
    })
}

fn snippet_at(body: &str, lower: &str, byte_pos: usize, len: usize) -> String {
    let char_pos = lower[..byte_pos.min(lower.len())].chars().count();
    let total = body.chars().count();
    let start = char_pos.saturating_sub(60);
    let end = (char_pos + len + 60).min(total);
    let slice: String = body.chars().skip(start).take(end.saturating_sub(start)).collect();
    let cleaned = slice.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(&cleaned);
    if end < total {
        s.push('…');
    }
    s
}
