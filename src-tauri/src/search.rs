//! 混合检索：结构化过滤 + 名称/路径/拼音/模糊 + 视觉标签 + 全文 + 语义 + 跨模态。
use crate::config::SearchConfig;
use crate::db::{Db, FileRow};
use crate::py::pinyin_match;
use crate::util::{build_fts_expr, format_ts, human_size, kind_label, now_ts};
use crate::vector::{normalize, VectorStore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Keep an explicit safety ceiling for a single UI request while removing the
/// old 200-result behavior. Larger result sets should be paged with `offset`.
// The database candidate scan currently supplies at most 50,000 rows. Keep
// the request ceiling aligned with that scan so the old 1,000-result UI cap
// cannot silently truncate otherwise valid matches.
const MAX_RESULT_LIMIT: usize = 50_000;
/// Final user-visible relevance floor shared by indexed, similar, and
/// workspace results. Internal model floors remain separate calibration steps.
pub const MIN_RESULT_SCORE: f32 = 0.68;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchQuery {
    pub text: String,
    pub kinds: Vec<String>,
    pub exts: Vec<String>,
    pub roots: Vec<String>,
    pub categories: Vec<String>,
    pub size_min: Option<i64>,
    pub size_max: Option<i64>,
    pub mtime_after: Option<i64>,
    pub mtime_before: Option<i64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// relevance | name | size | mtime | kind
    pub sort: Option<String>,
    pub desc: bool,
    pub use_fts: Option<bool>,
    pub use_pinyin: Option<bool>,
    pub use_fuzzy: Option<bool>,
    pub use_semantic: Option<bool>,
    /// Explicit retrieval mode selected by the UI.
    #[serde(default)]
    pub search_mode: Option<String>,
    /// 「用一句话描述找图片/声音」：查询词额外编码进跨模态空间。
    pub cross_modal: Option<bool>,
    /// 纯向量检索（相似检索）时由调用方填入。
    #[serde(skip)]
    pub query_vector: Option<Vec<f32>>,
    /// 查询向量属于哪个空间："text"（默认，llama/hash 文本空间）或 "media"。
    #[serde(skip)]
    pub vector_space: Option<String>,
    /// Stable identity of the encoder that produced a shared-space query.
    /// Media/page vectors are model-specific even when dimensions match.
    #[serde(skip)]
    pub vector_model_key: Option<String>,
    /// Stable identity of the CLIP encoder that produced `clip_query`.
    /// PDF page vectors are persisted in the same CLIP store across model
    /// changes, so a query must restrict synthetic page IDs to the matching
    /// model key before the top-k scan. Standalone media vectors do not yet
    /// carry a durable model key and remain compatible by file ID.
    #[serde(skip)]
    pub clip_model_key: Option<String>,
    /// 文字 → 图片空间（CLIP）。
    #[serde(skip)]
    pub clip_query: Option<Vec<f32>>,
    /// 文字 → 声音空间（CLAP）。
    #[serde(skip)]
    pub audio_query: Option<Vec<f32>>,
    /// 是否把「工作区目录」里尚未入库的文件也算进来（实时抽取正文）。
    pub workspace: Option<bool>,
    /// Search scope selected on the home page: `all` (all indexed rows plus
    /// configured live workspace), `indexed` (database only), or `path`
    /// (the selected path, including live files that are not indexed yet).
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub scope_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub id: i64,
    pub path: String,
    pub dir: String,
    pub name: String,
    pub ext: String,
    pub kind: String,
    pub kind_label: String,
    pub size: i64,
    pub size_label: String,
    pub mtime: i64,
    pub mtime_label: String,
    pub category: Option<String>,
    pub thumb: bool,
    /// Persistent thumbnail cache key. The UI must use this only when the
    /// indexed row confirms that the cache belongs to the current file.
    pub thumb_key: Option<String>,
    pub score: f32,
    pub reason: String,
    pub snippet: Option<String>,
    /// One-based page/slide number for page-level document matches.
    pub page: Option<i64>,
    /// Temporal location for a video segment match, in seconds from the start.
    pub start_seconds: Option<f64>,
    pub end_seconds: Option<f64>,
    /// Text subtitle matched or fused into the video segment embedding.
    pub subtitle: Option<String>,
    /// 视觉标签（图片/视频）：蓝天、草地、雪景、夕阳…
    pub tags: Option<String>,
}

impl Hit {
    fn from_row(r: &FileRow, score: f32, reason: &str) -> Self {
        Self {
            id: r.id,
            path: r.path.clone(),
            dir: r.dir.clone(),
            name: r.name.clone(),
            ext: r.ext.clone(),
            kind: r.kind.clone(),
            kind_label: kind_label(&r.kind).to_string(),
            size: r.size,
            size_label: human_size(r.size),
            mtime: r.mtime,
            mtime_label: format_ts(r.mtime),
            category: r.category.clone(),
            thumb: r.thumb_key.is_some(),
            thumb_key: r.thumb_key.clone(),
            score,
            reason: reason.to_string(),
            snippet: None,
            page: None,
            start_seconds: None,
            end_seconds: None,
            subtitle: None,
            tags: r.tags.clone(),
        }
    }
}

pub struct SearchEngine<'a> {
    pub db: &'a Db,
    /// 文本向量空间（llama.cpp GGUF / hash 文本）
    pub vectors: &'a VectorStore,
    /// 图片/音频/视频向量空间（hash 特征，维度独立）
    pub media_vectors: &'a VectorStore,
    /// 跨模态图片空间（CLIP 512 维）
    pub clip_vectors: &'a VectorStore,
    /// 跨模态音频空间（CLAP 512 维）
    pub audio_vectors: &'a VectorStore,
    pub cfg: &'a SearchConfig,
}

fn limit_for_cross(q: &SearchQuery) -> usize {
    // Over-fetch only a small margin beyond the requested page.  The old
    // 400-result floor made a first-page image/audio query scan hundreds of
    // vectors even when the UI asked for a much smaller page.  Background
    // pagination still increases the offset and therefore naturally widens
    // this window as more pages are requested.
    q.offset
        .unwrap_or(0)
        .saturating_add(q.limit.unwrap_or(800))
        .saturating_add(64)
        .min(MAX_RESULT_LIMIT)
}

/// Restrict a query to configured, enabled index roots while preserving a
/// caller's narrower root selection. `SearchEngine` is also used directly by
/// LAN and CLI entry points, so they must apply the same scope as AppState.
pub fn apply_enabled_root_scope(
    q: &mut SearchQuery,
    configured_roots: &[String],
    disabled_roots: &[String],
) {
    if configured_roots.is_empty() {
        return;
    }

    let enabled: Vec<String> = configured_roots
        .iter()
        .filter(|root| {
            !disabled_roots.iter().any(|disabled| {
                crate::util::norm_path(disabled)
                    .eq_ignore_ascii_case(&crate::util::norm_path(root))
            })
        })
        .map(|root| crate::util::norm_path(root))
        .collect();
    let requested = std::mem::take(&mut q.roots);
    let mut roots = Vec::new();
    for active in &enabled {
        let active_lower = active.to_ascii_lowercase();
        for requested_root in &requested {
            let requested_root = crate::util::norm_path(requested_root);
            let requested_lower = requested_root.to_ascii_lowercase();
            let active_prefix = format!("{}\\", active_lower.trim_end_matches('\\'));
            let requested_prefix = format!("{}\\", requested_lower.trim_end_matches('\\'));
            if active_lower == requested_lower || active_lower.starts_with(&requested_prefix) {
                roots.push(active.clone());
                break;
            }
            if requested_lower.starts_with(&active_prefix) {
                roots.push(requested_root);
                break;
            }
        }
        if requested.is_empty() {
            roots.push(active.clone());
        }
    }
    roots.sort_by_key(|root| root.to_ascii_lowercase());
    roots.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    q.roots = if roots.is_empty() {
        vec!["__deepdark_oracle_no_enabled_index_root__".into()]
    } else {
        roots
    };
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// A subtitle cue persisted in a video window's `subtitle` field.  Video
/// windows are still stored as one vector per 30 seconds, but the encoder now
/// includes cue boundaries in the text it receives.  Keeping this parser in
/// the search layer lets us expose the cue's real timestamp without changing
/// the database schema (and keeps old, untimestamped indexes compatible).
#[derive(Debug, Clone, PartialEq)]
struct IndexedSubtitleCue {
    start_ms: i64,
    end_ms: i64,
    text: String,
}

type SubtitleCueMatchRank = (u32, usize, usize);
type SemanticVideoMatch = (
    Option<SubtitleCueMatchRank>,
    f32,
    (i64, i64, String),
);

fn parse_subtitle_timestamp_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    let mut parts = value.split(':').collect::<Vec<_>>();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let seconds_part = parts.pop()?.trim();
    let seconds = seconds_part.parse::<f64>().ok()?;
    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let minutes = parts.pop()?.trim().parse::<i64>().ok()?;
    if !(0..60).contains(&minutes) {
        return None;
    }
    let hours = if let Some(hours) = parts.pop() {
        hours.trim().parse::<i64>().ok()?.max(0)
    } else {
        0
    };
    let millis = ((hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds) * 1000.0).round();
    if !millis.is_finite() || millis < 0.0 || millis > i64::MAX as f64 {
        return None;
    }
    Some(millis as i64)
}

/// Parse all `[MM:SS.mmm-MM:SS.mmm] text` markers emitted by the video
/// indexer.  Parsing is deliberately marker based rather than line based:
/// multiple cues are joined with spaces before they are persisted, so a single
/// subtitle value can contain several cues on one line.
fn parse_indexed_subtitle_cues(subtitle: &str) -> Vec<IndexedSubtitleCue> {
    let bytes = subtitle.as_bytes();
    let mut cursor = 0usize;
    let mut cues = Vec::new();
    while cursor < bytes.len() {
        let Some(open_rel) = subtitle[cursor..].find('[') else {
            break;
        };
        let open = cursor + open_rel;
        let Some(close_rel) = subtitle[open + 1..].find(']') else {
            break;
        };
        let close = open + 1 + close_rel;
        let marker = subtitle[open + 1..close].trim();
        let Some((start, end)) = marker.split_once('-') else {
            cursor = close.saturating_add(1);
            continue;
        };
        let (Some(start_ms), Some(end_ms)) = (
            parse_subtitle_timestamp_ms(start),
            parse_subtitle_timestamp_ms(end),
        ) else {
            cursor = close.saturating_add(1);
            continue;
        };
        if end_ms <= start_ms {
            cursor = close.saturating_add(1);
            continue;
        }
        let text_start = close + 1;
        let next_open = subtitle[text_start..]
            .find('[')
            .map(|offset| text_start + offset)
            .unwrap_or(subtitle.len());
        let text = subtitle[text_start..next_open]
            .trim_matches(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .trim()
            .to_string();
        if !text.is_empty() {
            cues.push(IndexedSubtitleCue {
                start_ms,
                end_ms,
                text,
            });
        }
        cursor = next_open;
    }
    cues
}

/// Rank the best subtitle cue in a video window for a textual query.  The
/// rank deliberately remains independent of the vector score: a clear cue
/// match in any candidate window should beat a higher-scoring window whose
/// subtitle does not contain the query.
fn subtitle_cue_match_rank(subtitle: &str, query: &str) -> Option<SubtitleCueMatchRank> {
    let query_lower = query.trim().to_lowercase();
    if query_lower.is_empty() {
        return None;
    }
    let query_terms = tokenize(&query_lower);
    parse_indexed_subtitle_cues(subtitle)
        .into_iter()
        .filter_map(|cue| {
            let cue_lower = cue.text.to_lowercase();
            let phrase = cue_lower.contains(&query_lower);
            let matched_terms = query_terms
                .iter()
                .filter(|term| cue_lower.contains(term.as_str()))
                .count();
            if !phrase && matched_terms == 0 {
                return None;
            }
            Some((
                if phrase { 3 } else { 2 },
                matched_terms,
                usize::MAX.saturating_sub(cue.text.chars().count()),
            ))
        })
        .max()
}

fn semantic_video_match_is_better(
    candidate: &SemanticVideoMatch,
    current: &SemanticVideoMatch,
) -> bool {
    candidate
        .0
        .cmp(&current.0)
        .then_with(|| candidate.1.total_cmp(&current.1))
        .is_gt()
}

/// Pick the cue most relevant to a textual query inside a matched video
/// window.  A vector tells us which 30-second window matched; cue-level text
/// then narrows the UI timestamp when the query terms occur in one of the
/// window's subtitles.  If no cue can be matched, the complete window range is
/// retained so old indexes and image/audio-only hits behave exactly as before.
fn refine_video_location(
    window_start_ms: i64,
    window_end_ms: i64,
    subtitle: &str,
    query: &str,
) -> (f64, f64, String) {
    let cues = parse_indexed_subtitle_cues(subtitle);
    if cues.is_empty() {
        return (
            window_start_ms as f64 / 1000.0,
            window_end_ms as f64 / 1000.0,
            subtitle.to_string(),
        );
    }
    let query_lower = query.trim().to_lowercase();
    let query_terms = tokenize(&query_lower);
    let mut best: Option<(SubtitleCueMatchRank, IndexedSubtitleCue)> = None;
    for cue in cues {
        let cue_lower = cue.text.to_lowercase();
        let phrase = !query_lower.is_empty() && cue_lower.contains(&query_lower);
        let matched_terms = query_terms
            .iter()
            .filter(|term| cue_lower.contains(term.as_str()))
            .count();
        if !phrase && matched_terms == 0 {
            continue;
        }
        // Exact phrases outrank term coverage; shorter matching cues make the
        // displayed timestamp more useful when several cues contain a term.
        let rank = (
            if phrase { 3 } else { 2 },
            matched_terms,
            usize::MAX.saturating_sub(cue.text.chars().count()),
        );
        if best.as_ref().is_none_or(|(current_rank, _)| rank > *current_rank) {
            best = Some((rank, cue));
        }
    }
    let Some((_, cue)) = best else {
        return (
            window_start_ms as f64 / 1000.0,
            window_end_ms as f64 / 1000.0,
            subtitle.to_string(),
        );
    };
    (
        cue.start_ms as f64 / 1000.0,
        cue.end_ms as f64 / 1000.0,
        cue.text,
    )
}

fn name_score(
    ql: &str,
    query_chars: usize,
    query_terms: &[String],
    name: &str,
) -> (f32, &'static str) {
    let nl = name.to_lowercase();
    if nl == ql {
        return (1.0, "名称精确匹配");
    }
    if nl.starts_with(ql) {
        return (0.92, "名称前缀匹配");
    }
    if nl.contains(ql) {
        let cover = query_chars as f32 / nl.chars().count().max(1) as f32;
        return (0.72 + 0.26 * cover.min(1.0), "名称包含");
    }
    // 逐词命中
    if query_terms.len() > 1 {
        let hit = query_terms.iter().filter(|t| nl.contains(t.as_str())).count();
        if hit > 0 {
            let ratio = hit as f32 / query_terms.len() as f32;
            return (0.55 + 0.2 * ratio, "名称多词命中");
        }
    }
    (0.0, "")
}

fn fuzzy_score(ql: &str, query_chars: usize, name: &str) -> f32 {
    if query_chars < 3 {
        return 0.0;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let sim = strsim::jaro_winkler(ql, &stem);
    if sim >= 0.80 {
        0.35f32 + 0.35 * ((sim - 0.80) as f32 / 0.20)
    } else {
        0.0f32
    }
}

fn semantic_score(similarity: f32, configured_floor: f32, query_chars: usize) -> Option<f32> {
    if !similarity.is_finite() {
        return None;
    }
    // Very short queries (especially single Chinese characters) have weak
    // embedding intent and otherwise return broad clusters of unrelated
    // images. Keep the configured floor for normal queries, with a stricter
    // minimum for ambiguous short input.
    // Two-character CJK terms have a very broad embedding neighbourhood.
    // Requiring a little more cosine similarity removes weak neighbours such
    // as 「活塞」→「油漆」 while preserving multilingual exact concepts.
    // Very short queries are especially prone to broad embedding neighbours
    // (for example a two-character part name such as “活塞” matching an
    // unrelated “油漆” page).  Keep a deliberately conservative floor for
    // these queries.  The lexical/FTS lane still handles exact text matches,
    // while semantic-only results must demonstrate stronger evidence.
    let short_query_floor = if query_chars <= 2 { 0.76 } else { 0.0 };
    // A raw cosine is a model-specific distance, not a probability. Keep it
    // unchanged as the semantic evidence; exact text and lexical coverage
    // provide the calibrated relevance tiers below.
    let floor = configured_floor.clamp(0.0, 0.99).max(short_query_floor).max(MIN_RESULT_SCORE);
    if similarity < floor {
        return None;
    }
    Some(calibrate_semantic_relevance(similarity.clamp(0.0, 1.0)))
}

/// Image vectors from a shared multimodal encoder have a different cosine
/// distribution from text-to-text vectors. Do not apply the short-query
/// semantic floor (0.76) to an explicit image query; that suppresses valid
/// cross-modal matches. Keep a configurable raw evidence floor, then map the
/// surviving cosine to the same visible score band as the dedicated CLIP lane.
fn image_semantic_score(similarity: f32, configured_floor: f32) -> Option<f32> {
    let floor = configured_floor.clamp(0.30, 0.65);
    if !similarity.is_finite() || similarity < floor {
        return None;
    }
    let ceiling = 0.82f32;
    let relative = ((similarity.clamp(floor, 1.0) - floor) / (ceiling - floor)).clamp(0.0, 1.0);
    Some(0.68 + relative * 0.28)
}

fn mode_semantic_score(
    similarity: f32,
    configured_floor: f32,
    query_chars: usize,
    image_mode: bool,
) -> Option<f32> {
    if image_mode {
        image_semantic_score(similarity, configured_floor)
    } else {
        semantic_score(similarity, configured_floor, query_chars)
    }
}

/// A temporal mapping proves where a vector came from, not that it matches
/// the query. Shared-encoder video vectors must pass the same evidence gate
/// in every lane; otherwise image/audio searches can reintroduce videos that
/// the semantic lane rejected. Exact subtitle matches use the independent
/// full-text path and do not need to pass this vector-only gate.
fn video_semantic_score(
    similarity: f32,
    configured_floor: f32,
    query_chars: usize,
) -> Option<f32> {
    semantic_score(similarity, configured_floor, query_chars)
}

/// Convert backend cosine into a user-facing relevance scale. Cosine is not a
/// probability, and values above the noise floor tend to bunch together. Keep
/// the 0.70 anchor stable while spreading stronger matches into clearer tiers;
/// lexical exact matches still own the only unconditional 1.0 score.
fn calibrate_semantic_relevance(cosine: f32) -> f32 {
    if cosine <= 0.70 {
        cosine
    } else if cosine <= 0.85 {
        0.70 + (cosine - 0.70) * (0.20 / 0.15)
    } else {
        0.90 + (cosine - 0.85) * (0.09 / 0.15)
    }
}

fn lexical_relevance_score(exact_phrase: bool, matched_terms: usize, total_terms: usize) -> Option<f32> {
    // Only a contiguous phrase is an exact lexical match. Complete coverage
    // across separated terms is useful evidence, but it must leave room for
    // semantic context to distinguish e.g. an engine piston from a pump or
    // gearbox piston.
    if exact_phrase {
        return Some(1.0);
    }
    if total_terms == 0 || matched_terms == 0 {
        return None;
    }
    let coverage = (matched_terms.min(total_terms) as f32 / total_terms as f32).clamp(0.0, 1.0);
    // Full term coverage without the literal phrase is a strong related hit,
    // while partial coverage stays around the broader semantic tier.
    let score = 0.52 + 0.36 * coverage;
    Some(if matched_terms >= total_terms { score.min(0.88) } else { score })
}

/// Score document FTS hits in the mixed retrieval view.  `build_fts_expr`
/// intentionally uses OR so a query can still recover partial evidence, but
/// a multi-term query must share most of its terms before it is allowed onto
/// the first page.  Otherwise a query such as "发动机活塞" is reduced to the
/// broad single-term hit "活塞" and unrelated pump/gearbox pages crowd out
/// the semantic context supplied by the embedding model.
fn contextual_lexical_relevance_score(
    exact_phrase: bool,
    matched_terms: usize,
    total_terms: usize,
) -> Option<f32> {
    if total_terms >= 2 && !exact_phrase {
        let required = (total_terms.saturating_sub(1)).max(2);
        if matched_terms < required {
            return None;
        }
    }
    let score = lexical_relevance_score(exact_phrase, matched_terms, total_terms)?;
    if total_terms >= 2 && !exact_phrase {
        // Co-occurring terms do not prove that the words describe the same
        // object. Keep this as medium lexical evidence until semantic search
        // confirms the relationship (for example, which subsystem a piston
        // belongs to).
        Some(score.min(0.70))
    } else {
        Some(score)
    }
}

/// Merge an FTS body score into a file that may already have been seeded by a
/// filename/path/fuzzy lane. Body coverage is authoritative for a content
/// result; retaining an unrelated seed score makes the UI report a larger
/// number than the actual matched-term coverage.
fn apply_fts_match(score: &mut f32, reason: &mut String, add: f32) {
    if reason == "全文内容匹配" {
        *score = score.max(add);
    } else if reason != "名称精确匹配" && reason != "名称前缀匹配" {
        *score = add;
        *reason = "全文内容匹配".to_string();
    }
}

fn fuse_relevance(lexical: f32, semantic: Option<f32>, semantic_weight: f32) -> f32 {
    if lexical >= 1.0 {
        return 1.0;
    }
    match semantic {
        Some(cosine) => {
            let weight = semantic_weight.clamp(0.0, 1.0);
            lexical.max(lexical * (1.0 - weight) + cosine * weight)
        }
        None => lexical,
    }
}

impl<'a> SearchEngine<'a> {
    pub fn search(&self, q: &SearchQuery) -> Vec<Hit> {
        let text = q.text.trim().to_string();
        let lower_text = text.to_lowercase();
        let query_terms = tokenize(&text);
        let query_chars = text.chars().count();
        let limit = q.limit.unwrap_or(self.cfg.page_size).clamp(1, MAX_RESULT_LIMIT);
        let offset = q.offset.unwrap_or(0);
        // Keep the first request proportional to the page the UI requested.
        // A bounded margin gives the de-duplication/aggregation lanes a few
        // extra candidates without forcing every keystroke to scan 500 or
        // 800 rows.  Later background pages grow through `offset`.
        let candidate_limit = offset
            .saturating_add(limit)
            .saturating_add(64)
            .min(MAX_RESULT_LIMIT);
        // An empty query is the only case that should disable lexical/vector
        // matching.  Single-character CJK terms are indexed explicitly by
        // `cjk_bigrams`, so treating them as "low text" made valid searches
        // such as a one-character part name return no results at all.
        // Semantic scoring still applies its stricter short-query floor below.
        let low_text = query_chars == 0;
        let mode = q.search_mode.as_deref().unwrap_or("hybrid");
        let fulltext_mode = mode.eq_ignore_ascii_case("fulltext");
        let semantic_mode = mode.eq_ignore_ascii_case("semantic");
        let image_mode = mode.eq_ignore_ascii_case("image");
        let audio_mode = mode.eq_ignore_ascii_case("audio");
        let explicit_mode = fulltext_mode || semantic_mode || image_mode || audio_mode;
        // 相似检索：只有向量、没有文本
        let pure_vector = q.query_vector.is_some() && low_text;
        // In explicit full-text mode, FTS5 already selects active files. Skip
        // the broad candidate scan unless structural filters require it.
        // This avoids loading up to 50,000 rows before the first text hit.
        // The UI asks for a small first page (normally 500 rows), while the
        // old hybrid path still loaded up to 50,000 file records before it
        // scored a single name/path candidate.  That broad read became a
        // measurable part of every search on large indexes.  Keep enough
        // headroom for filename/pinyin/fuzzy matches, but tie the scan to
        // the requested page and cap it at a bounded size.  FTS and vector
        // lanes remain independent and can still return their full bounded
        // result page.
        let candidate_scan_limit = if low_text {
            candidate_limit.max(2_048).min(20_000)
        } else if has_filters(q) {
            candidate_limit.max(2_048).min(20_000)
        } else {
            candidate_limit.max(4_096).min(10_000)
        };
        let rows = if explicit_mode && !has_filters(q) {
            Vec::new()
        } else {
            self.candidate_rows(q, candidate_scan_limit)
        };
        let mut scored: HashMap<i64, (FileRow, f32, String)> = HashMap::new();
        let mut matched_pages: HashMap<i64, i64> = HashMap::new();
        let mut matched_videos: HashMap<i64, (i64, i64, String)> = HashMap::new();
        // A file-level semantic vector cannot identify an exact page by
        // itself. When the query terms also occur in the paged FTS store, use
        // that strongest page as a precise navigation hint instead of
        // dropping the page location for semantic results.
        let semantic_page_hints: HashMap<i64, i64> = if semantic_mode && !text.is_empty() {
            let terms = crate::util::fts_query_terms(&text);
            build_fts_expr(&text)
                .and_then(|expr| {
                    self.db
                        .search_pages_fts_relevance(&expr, &text, &terms, candidate_limit)
                        .ok()
                })
                .unwrap_or_default()
                .into_iter()
                .map(|(file_id, page, _, _, _)| (file_id, page))
                .collect()
        } else {
            HashMap::new()
        };

        // Explicit full-text mode must be driven by FTS hits only.  The old
        // path also seeded filename/name scores before FTS and then relabeled
        // those candidates as “全文匹配”, which produced misleading values
        // such as 0.75 for a file whose body did not contain the query.
        if !explicit_mode && !low_text {
            let use_pinyin = q.use_pinyin.unwrap_or(self.cfg.use_pinyin);
            let use_fuzzy = q.use_fuzzy.unwrap_or(self.cfg.use_fuzzy);
            for r in &rows {
                let (mut score, mut reason) =
                    name_score(&lower_text, query_chars, &query_terms, &r.name);
                if score == 0.0 {
                    let pl = norm_path_lower(&r.path);
                    if pl.contains(&lower_text) {
                        let cover = query_chars as f32 / pl.chars().count().max(1) as f32;
                        score = 0.50 + 0.15 * cover.min(1.0);
                        reason = "路径匹配";
                    }
                }
                if score == 0.0 && use_pinyin {
                    let ps = pinyin_match(&text, &r.name, &r.path);
                    if ps > 0.0 {
                        score = ps;
                        reason = "拼音匹配";
                    }
                }
                if score == 0.0 && use_fuzzy {
                    let fs = fuzzy_score(&lower_text, query_chars, &r.name);
                    if fs > 0.0 {
                        score = fs;
                        reason = "模糊匹配";
                    }
                }
                // 视觉标签：图片/视频索引时写入的中文关键词（蓝天/草地/雪景…）。
                // 命中一个标签给 0.62，命中越多越高，满分 0.82 —— 低于文件名
                // 精确匹配（1.0），高于路径匹配（0.5~0.65），符合「用户描述的是
                // 画面内容，不是文件名」的预期。
                if score < 0.62 {
                    if let Some(tags) = &r.tags {
                        let parts: Vec<&str> = tags.split(',').filter(|s| !s.is_empty()).collect();
                        let hit = parts
                            .iter()
                            .filter(|t| {
                                let tag = t.to_lowercase();
                                tag.contains(&lower_text) || lower_text.contains(&tag)
                            })
                            .count();
                        if hit > 0 {
                            let s = 0.62 + 0.20 * (hit as f32 / parts.len().max(1) as f32).min(1.0);
                            if s > score {
                                score = s;
                                reason = "画面标签";
                            }
                        }
                    }
                }
                if score > 0.0 {
                    scored.insert(r.id, (r.clone(), score, reason.to_string()));
                }
            }
        }

        // 空查询（只按类型/大小/时间筛选，或刚打开界面）时要能「浏览」，
        // 否则列表永远为空，用户会以为索引坏了。
        if query_chars == 0 && q.query_vector.is_none() && !explicit_mode {
            for r in &rows {
                scored.insert(r.id, (r.clone(), 0.5, "全部".to_string()));
            }
        }

        // 全文
        let use_fts = q.use_fts.unwrap_or(self.cfg.use_fulltext)
            && (!explicit_mode || fulltext_mode);
        let mut fts_ids: HashSet<i64> = HashSet::new();
        if !low_text && (use_fts || semantic_mode) {
            // Semantic mode still needs the subtitle FTS lane for videos, but
            // must not turn ordinary document FTS hits into semantic results.
            // Build the normalized terms once so both lanes use the same
            // tokenization without an extra SQLite query or allocation.
            let mut fts_terms = crate::util::fts_query_terms(&text);
            // The normal tokenizer drops very short ASCII words.  Keep one
            // literal term for the subtitle lane (and raw document fallback)
            // so short queries can still match persisted video text.
            if fts_terms.is_empty() && !text.trim().is_empty() {
                fts_terms.push(text.trim().to_lowercase());
            }
            if use_fts || semantic_mode {
                if let Some(expr) = build_fts_expr(&text) {
                // Ordinary document/page FTS must remain exclusive to the
                // full-text lane. Semantic mode enters this outer block only
                // so that its timestamped video-subtitle lane below still
                // runs when a video has no vector hit.
                if use_fts {
                // Page-aware FTS is authoritative for paged documents. The
                // aggregate `docs` row can report all query terms as present
                // even when each term lives on a different PDF page, which
                // falsely upgrades a result to an exact 1.0 match. Keep the
                // strongest page hit (and its page number) so a PDF is scored
                // from one logical page at a time.
                let page_hits = self
                    .db
                    .search_pages_fts_relevance(
                        &expr,
                        &text,
                        &fts_terms,
                        candidate_limit,
                    )
                    .unwrap_or_default();
                let page_relevance: HashMap<i64, (i64, bool, usize)> = page_hits
                    .iter()
                    .map(|(fid, page, _rank, exact, matched)| {
                        (*fid, (*page, *exact, *matched))
                    })
                    .collect();
                if let Ok(hits) = self.db.search_fts_relevance(
                    &expr,
                    &text,
                    &fts_terms,
                    candidate_limit,
                ) {
                    // bm25() 返回的是负数，越负越相关（ORDER BY 升序 = 最相关在前）。
                    // 所以取反后再归一化，否则最相关的那条 rel 会是 0、其余为负，
                    // 全部被阈值滤掉 —— 表现为「正文里明明有这个词却搜不到」。
                    // Video subtitles live in `video_segments`, not the
                    // aggregate `docs` FTS table. Keep this block active for
                    // a non-empty query even when the document FTS result is
                    // empty, otherwise subtitle-only videos can never be
                    // returned.
                    if !hits.is_empty() || !text.is_empty() {
                        // 带结构化筛选（类型/扩展名/目录/大小/时间）时，全文命中
                        // 也必须落在候选集内，否则用户筛了「图片」却冒出 PDF。
                        let filtered = has_filters(q);
                        let allowed: HashSet<i64> = if filtered {
                            rows.iter().map(|r| r.id).collect()
                        } else {
                            HashSet::new()
                        };
                        // Resolve all FTS file IDs in one batched SQLite query
                        // instead of acquiring the DB mutex once per hit.
                        let aggregate_ids: Vec<i64> = hits.iter().map(|(fid, _, _, _)| *fid).collect();
                        let paged_ids = self.db.paged_file_ids(&aggregate_ids);
                        let fts_rows: HashMap<i64, FileRow> = if filtered {
                            // Explicit full-text mode does not pre-seed
                            // `scored`. A scope/type filter must still supply
                            // its allowed rows for FTS hits to be inserted.
                            rows.iter().map(|row| (row.id, row.clone())).collect()
                        } else {
                            let ids: Vec<i64> = hits.iter().map(|(fid, _, _, _)| *fid).collect();
                            self.db
                                .files_by_ids(&ids)
                                .into_iter()
                                .map(|row| (row.id, row))
                                .collect()
                        };
                        for (fid, _bm25, aggregate_exact, aggregate_terms) in hits {
                            if filtered && !allowed.contains(&fid) {
                                continue;
                            }
                            let Some((exact_phrase, matched_terms)) = page_relevance
                                .get(&fid)
                                .map(|(_, exact, matched)| (*exact, *matched))
                                .or_else(|| {
                                    // A paged document must be scored from one
                                    // logical page. Do not fall back to the
                                    // aggregate row when the page candidate
                                    // window was truncated.
                                    (!paged_ids.contains(&fid))
                                        .then_some((aggregate_exact, aggregate_terms))
                                }) else {
                                continue;
                            };
                            // The default hybrid endpoint also starts with a
                            // model-free FTS pass. Apply the same page-local
                            // context rule there; otherwise legacy callers
                            // could still turn a single component such as
                            // "活塞" into a broad hit before semantic recall
                            // has a chance to disambiguate it.
                            let score_fn = if fulltext_mode || !explicit_mode {
                                contextual_lexical_relevance_score
                            } else {
                                lexical_relevance_score
                            };
                            let Some(add) = score_fn(exact_phrase, matched_terms, fts_terms.len()) else {
                                continue;
                            };
                            fts_ids.insert(fid);
                            if let Some((_row, score, reason)) = scored.get_mut(&fid) {
                                // A content hit must not inherit a larger
                                // filename/path/fuzzy score. Doing so made a
                                // card labelled “全文匹配” show 0.75 even
                                // though only a partial body-term match was
                                // worth 0.70. Keep an existing content score
                                // when several FTS rows point to the same
                                // file; otherwise replace the unrelated
                                // pre-seed score with the authoritative FTS
                                // coverage score.
                                apply_fts_match(score, reason, add);
                            } else if let Some(r) = fts_rows.get(&fid).cloned() {
                                // 全文表里的孤儿行（文件已删）不该冒出来；
                                // candidate_rows 已经过滤 missing，这里补一道。
                                if r.path.is_empty() {
                                    continue;
                                }
                                scored.insert(fid, (r, add, "全文内容匹配".to_string()));
                            }
                        }
                    }
                    // Preserve the strongest matching page for the result
                    // payload. This uses the same page-aware rows that drove
                    // the score above, keeping score and displayed page in
                    // sync.
                    for (fid, page, _rank, _exact, _matched) in page_hits {
                        matched_pages.entry(fid).or_insert(page);
                }
                }
                }

                    // Video subtitles are stored per 30-second window rather
                    // than in the aggregate `docs` FTS table. Search them in
                    // the same full-text lane so a spoken phrase returns the
                    // video and its precise cue timestamp.
                    // Subtitle FTS is only relevant when the current
                    // structural scope can contain videos.  Previously an
                    // image/document/audio-only filter still scanned every
                    // video window and discarded the rows afterwards, which
                    // made ordinary searches slow as the video library grew.
                    if video_candidates_allowed(q, &rows) {
                        if let Ok(video_hits) = self.db.search_video_subtitle_relevance(
                            &text,
                            &fts_terms,
                            candidate_limit,
                        ) {
                        let filtered = has_filters(q);
                        let allowed: HashSet<i64> = if filtered {
                            rows.iter().map(|r| r.id).collect()
                        } else {
                            HashSet::new()
                        };
                        let ids: Vec<i64> = video_hits.iter().map(|hit| hit.file_id).collect();
                        let video_rows: HashMap<i64, FileRow> = self
                            .db
                            .files_by_ids(&ids)
                            .into_iter()
                            .map(|row| (row.id, row))
                            .collect();
                        for hit in video_hits {
                            if filtered && !allowed.contains(&hit.file_id) {
                                continue;
                            }
                            // Subtitle windows are temporal candidates too.
                            // Apply the same page/window-local context gate as
                            // document FTS so a video containing only one
                            // component of a multi-word query is not promoted
                            // as a full-text video match.
                            let Some(add) = contextual_lexical_relevance_score(
                                hit.exact_phrase,
                                hit.matched_terms,
                                fts_terms.len(),
                            ) else {
                                continue;
                            };
                            // SQL orders each file's best cue first (phrase,
                            // term coverage, then shorter text), so retaining
                            // the first cue gives stable per-file selection.
                            matched_videos
                                .entry(hit.file_id)
                                .or_insert((hit.start_ms, hit.end_ms, hit.subtitle));
                            fts_ids.insert(hit.file_id);
                            if let Some((_row, score, reason)) = scored.get_mut(&hit.file_id) {
                                *score = score.max(add);
                                if reason != "鍚嶇О绮剧‘鍖归厤" && reason != "鍚嶇О鍓嶇紑鍖归厤" {
                                    *reason = "鍏ㄦ枃鍐呭鍖归厤".to_string();
                                }
                            } else if let Some(row) = video_rows.get(&hit.file_id).cloned() {
                                scored.insert(
                                    hit.file_id,
                                    (row, add, "鍏ㄦ枃鍐呭鍖归厤".to_string()),
                                );
                            }
                        }
                        }
                    }
                }
            }
        }

        // 语义
        // Explicit image/audio lanes must not fall back to unrelated text
        // embeddings. Image searches may use the shared primary space only
        // when the selected encoder produced a shared multimodal query;
        // otherwise CLIP is the image lane. Audio uses CLAP and, for an
        // explicitly shared encoder, the matching shared media vectors.
        let use_semantic = q.use_semantic.unwrap_or(self.cfg.use_semantic)
            && (!explicit_mode
                || semantic_mode
                || (image_mode
                    && matches!(q.vector_space.as_deref(), Some("shared") | Some("media")))
                || (audio_mode
                    && matches!(q.vector_space.as_deref(), Some("shared") | Some("media"))
                    && q.vector_model_key.is_some()));
        if let Some(qv) = &q.query_vector {
            // Explicit modality searches only use a matching modality space.
            // Candidate IDs below keep text and unrelated page vectors out.
            if use_semantic || pure_vector {
                let mut qvec = qv.clone();
                normalize(&mut qvec);
                // 不能用 scored.keys()（已命中的），否则语义就退化成「重排」：
                // 查询词与正文完全不重叠时（纯语义提问）scored 为空，
                // allowed 为空集，语义永远召不回任何文件。
                // 这里用「结构化过滤后的候选集」，既尊重类型/路径/大小筛选，又能召回。
                let allowed: HashSet<i64> = rows.iter().map(|r| r.id).collect();
                // With no structural filters, the vector store is already
                // the candidate index. Avoid reading tens of thousands of
                // file rows just to build an allow-list that contains every
                // file; source rows are resolved in one batch below.
                let allowed_ref = if pure_vector || !has_filters(q) {
                    None
                } else {
                    Some(&allowed)
                };
                // 传统后端的文本与媒体空间维度不同，不能混查；
                // Candle 多模态模型显式声明 shared 后才会同时搜索两个空间。
                // Candle shares one vector space for text and media. Legacy
                // backends keep separate dimensions and search one store.
                // A text-only primary encoder leaves `vector_space` unset,
                // but videos indexed by the dedicated multimodal fallback
                // live in `media_vectors`.  Probe only the durable temporal
                // IDs in that store for semantic/hybrid queries.  Keeping the
                // allow-list narrow avoids scanning CLIP/CLAP/file-level media
                // vectors and prevents unrelated media rows from slowing or
                // contaminating ordinary text search.
                let stores: Vec<&VectorStore> = match q.vector_space.as_deref() {
                    // A shared multimodal model stores pure PDF image vectors
                    // in the primary store. Non-shared text embeddings cannot
                    // be compared with those vectors.
                    Some("shared") if image_mode && q.vector_model_key.is_some() => {
                        // A multimodal model may have indexed temporal video
                        // windows in the media store when it exposes audio or
                        // image input without a native video tower.  Search
                        // both stores; each store still applies the exact
                        // model-key and dimension allow-list below.
                        vec![self.vectors, self.media_vectors]
                    }
                    // A real media embedder can keep image/video windows in
                    // the dedicated media store even when the primary text
                    // model has no shared modality.  This branch is reached
                    // by the explicit fallback lane in `cmd.rs`.
                    Some("media") if image_mode || audio_mode => vec![self.media_vectors],
                    Some(_) if image_mode => Vec::new(),
                    // Shared multimodal encoders can store audio-file and
                    // video-window embeddings in the primary vector store.
                    // Legacy, non-shared encoders use CLAP below instead.
                    Some("shared") if audio_mode && q.vector_model_key.is_some() => {
                        vec![self.vectors, self.media_vectors]
                    }
                    Some(_) if audio_mode => Vec::new(),
                    // A missing space declaration must not fall through to
                    // the primary text store for explicit modality searches.
                    // This can happen for callers that supply a query vector
                    // directly; comparing it with a media file's ordinary
                    // text embedding would create a false image/audio hit.
                    None if image_mode || audio_mode => Vec::new(),
                    Some("media") => vec![self.media_vectors],
                    Some("shared") => vec![self.vectors, self.media_vectors],
                    _ => vec![self.vectors],
                };
                // A shared vector store also contains text page/file vectors.
                // Searching its global top-k and filtering afterward can let
                // text vectors crowd all PDF image pages out of the candidate
                // set. Restrict image-mode candidates up front to image/video
                // files and PDF image pages generated by this exact encoder.
                let shared_image_ids = if image_mode
                    && q.vector_space.as_deref() == Some("shared")
                {
                    q.vector_model_key.as_deref().map(|key| {
                        let mut ids: HashSet<i64> = self
                            .db
                            .doc_image_vector_ids_for_model_with_legacy(
                                key, self.vectors.accepts_unmarked_vectors(),
                            )
                            .unwrap_or_default()
                            .into_iter()
                            .collect();
                        // With no structural filters `rows` is intentionally
                        // empty (the vector store is the candidate index).
                        // Load standalone image IDs from the vector store in
                        // that case, while retaining the filtered file list
                        // when the user selected a scope/type constraint.
                        if rows.is_empty() && !has_filters(q) {
                            let standalone_ids: HashSet<i64> = self
                                .db
                                .active_file_ids_by_kinds(&["image"])
                                .unwrap_or_default()
                                .into_iter()
                                .collect();
                            ids.extend(
                                self.vectors
                                    .all_ids()
                                    .into_iter()
                                    .filter(|id| *id >= 0)
                                    .filter(|id| standalone_ids.contains(id)),
                            );
                        } else {
                            ids.extend(
                                rows.iter()
                                    .filter(|row| row.kind == "image")
                                    .map(|row| row.id),
                            );
                        }
                        // Shared multimodal video embeddings are stored per
                        // temporal window, not under the file ID. Admit only
                        // completed windows from this exact encoder and store
                        // dimension so image queries can resolve a frame hit
                        // back to its video and timestamp without mixing old
                        // model vectors.
                        let video_key = crate::index::video_model_key(key);
                        if rows.is_empty() && !has_filters(q) {
                            ids.extend(
                                video_ids_for_query(
                                    self.db,
                                    "primary",
                                    Some(key),
                                    self.vectors.dim(),
                                )
                                .into_iter()
                                .filter(|vector_id| self.vectors.has(*vector_id)),
                            );
                            ids.extend(
                                video_ids_for_query(
                                    self.db,
                                    "media",
                                    Some(key),
                                    self.media_vectors.dim(),
                                )
                                .into_iter()
                                .filter(|vector_id| self.media_vectors.has(*vector_id)),
                            );
                        } else {
                            for row in rows.iter().filter(|row| row.kind == "video") {
                                if let Ok(segments) = self.db.completed_video_segments_compatible(
                                    row.id,
                                    "primary",
                                    &video_key,
                                    self.vectors.dim(),
                                ) {
                                    ids.extend(segments.into_iter().filter_map(|segment| {
                                        usize::try_from(segment)
                                            .ok()
                                            .map(|segment| crate::ai::video_segment_id(row.id, segment))
                                            .filter(|vector_id| self.vectors.has(*vector_id))
                                    }));
                                }
                                if let Ok(segments) = self.db.completed_video_segments_compatible(
                                    row.id,
                                    "media",
                                    &video_key,
                                    self.media_vectors.dim(),
                                ) {
                                    ids.extend(segments.into_iter().filter_map(|segment| {
                                        usize::try_from(segment)
                                            .ok()
                                            .map(|segment| crate::ai::video_segment_id(row.id, segment))
                                            .filter(|vector_id| self.media_vectors.has(*vector_id))
                                    }));
                                }
                            }
                        }
                        ids
                    })
                } else {
                    None
                };
                // The shared primary store also contains ordinary document
                // text vectors. Filter audio searches before top-k so those
                // vectors cannot crowd out audio files and video windows.
                // Video windows are admitted only when their durable SQLite
                // record matches the selected encoder key and vector size;
                // legacy windows without that identity cannot be compared
                // safely with the current shared space.
                let shared_audio_ids = if audio_mode
                    && q.vector_space.as_deref() == Some("shared")
                {
                    q.vector_model_key.as_deref().map(|model_key| {
                        let mut ids = HashSet::new();
                        let video_key = crate::index::video_model_key(model_key);
                        if rows.is_empty() && !has_filters(q) {
                            let standalone_ids: HashSet<i64> = self
                                .db
                                .active_file_ids_by_kinds(&["audio", "video"])
                                .unwrap_or_default()
                                .into_iter()
                                .collect();
                            ids.extend(
                                self.vectors
                                    .all_ids()
                                    .into_iter()
                                    .filter(|id| *id >= 0)
                                    .filter(|id| standalone_ids.contains(id)),
                            );
                            ids.extend(
                                video_ids_for_query(
                                    self.db,
                                    "primary",
                                    Some(model_key),
                                    self.vectors.dim(),
                                )
                                .into_iter()
                                .filter(|vector_id| self.vectors.has(*vector_id)),
                            );
                            ids.extend(
                                video_ids_for_query(
                                    self.db,
                                    "media",
                                    Some(model_key),
                                    self.media_vectors.dim(),
                                )
                                .into_iter()
                                .filter(|vector_id| self.media_vectors.has(*vector_id)),
                            );
                        } else {
                            for row in rows.iter().filter(|row| {
                                matches!(row.kind.as_str(), "audio" | "video")
                            }) {
                                if self.vectors.has(row.id) {
                                    ids.insert(row.id);
                                }
                                if row.kind == "video" {
                                    if let Ok(segments) = self.db.completed_video_segments_compatible(
                                        row.id,
                                        "primary",
                                        &video_key,
                                        self.vectors.dim(),
                                    ) {
                                        ids.extend(segments.into_iter().filter_map(|segment| {
                                            usize::try_from(segment)
                                                .ok()
                                                .map(|segment| crate::ai::video_segment_id(row.id, segment))
                                                .filter(|vector_id| self.vectors.has(*vector_id))
                                        }));
                                    }
                                    if let Ok(segments) = self.db.completed_video_segments_compatible(
                                        row.id,
                                        "media",
                                        &video_key,
                                        self.media_vectors.dim(),
                                    ) {
                                        ids.extend(segments.into_iter().filter_map(|segment| {
                                            usize::try_from(segment)
                                                .ok()
                                                .map(|segment| crate::ai::video_segment_id(row.id, segment))
                                                .filter(|vector_id| self.media_vectors.has(*vector_id))
                                        }));
                                    }
                                }
                            }
                        }
                        ids
                    })
                } else {
                    None
                };
                // A shared semantic search normally scans the vector store's
                // global top-k. Ordinary text/page vectors can crowd all
                // temporal video windows out before they are mapped back to
                // their source file. Keep the normal search for text results,
                // then run a bounded second pass over the durable video-window
                // IDs produced by this exact model and merge the candidates.
                // A kind-filtered request whose candidate rows contain no
                // videos cannot benefit from the temporal-window priority
                // pass.  Building the compatible-window allow-list performs
                // two SQLite scans and is measurable on large indexes, so
                // skip it while preserving the default all-kinds behavior.
                let video_candidates_allowed = video_candidates_allowed(q, &rows);
                let shared_video_ids = if video_candidates_allowed
                    && !image_mode
                    && !audio_mode
                    && matches!(q.vector_space.as_deref(), Some("shared") | Some("media"))
                {
                    Some(
                        video_ids_for_query(
                            self.db,
                            "primary",
                            q.vector_model_key.as_deref(),
                            self.vectors.dim(),
                        )
                        .into_iter()
                        .filter(|vector_id| self.vectors.has(*vector_id))
                        .chain(
                            video_ids_for_query(
                                self.db,
                                "media",
                                q.vector_model_key.as_deref(),
                                self.media_vectors.dim(),
                            )
                            .into_iter()
                            .filter(|vector_id| self.media_vectors.has(*vector_id)),
                        )
                        .collect::<HashSet<_>>(),
                    )
                } else {
                    None
                };
                // Keep the user-visible floor at 0.40. Older configs used
                // 0.45 as an internal noise floor, which silently discarded
                // valid cross-language matches from multilingual embedders.
                let mut semantic_matches: HashMap<i64, (f32, Option<i64>, Option<(i64, i64, String)>)> = HashMap::new();
                let mut best_page_matches: HashMap<i64, (f32, i64)> = HashMap::new();
                let mut best_video_matches: HashMap<i64, SemanticVideoMatch> = HashMap::new();
                let mut semantic_source_rows: HashMap<i64, FileRow> = HashMap::new();
                // A shared text query only needs the dedicated media store
                // when this encoder actually produced compatible temporal
                // video windows.  The primary store is the normal home for
                // shared text/page vectors; scanning an unrelated or stale
                // media store on every keystroke doubled the exact cosine
                // work and was a major source of search latency.  Explicit
                // image/audio lanes still search both stores, and a media
                // query is already restricted to the media store above.
                let scan_media_store = !semantic_mode
                    || image_mode
                    || audio_mode
                    || q.vector_space.as_deref() == Some("media")
                    || shared_video_ids
                        .as_ref()
                        .is_some_and(|ids| !ids.is_empty());
                for store in stores {
                    if !scan_media_store && std::ptr::eq(store, self.media_vectors) {
                        continue;
                    }
                    // Page and video vectors use synthetic IDs. Resolve them
                    // before scope filtering and aggregate matches by source file.
                    // Resolve all mappings in bounded batches; resolving each
                    // candidate through SQLite individually made semantic
                    // search an N+1 query workload.
                    let vector_limit = candidate_limit;
                    let vector_hits = if let Some(video_ids) = shared_video_ids.as_ref()
                        .filter(|ids| !ids.is_empty())
                    {
                        // Shared primary stores contain ordinary text/page
                        // vectors as well as temporal video windows. Keep a
                        // global heap there so text results remain available.
                        // The dedicated media store, however, is only needed
                        // for those temporal windows during a normal semantic
                        // query. Scanning every image/audio/media vector in
                        // that store doubled the dot-product work on each
                        // keystroke and made search latency grow with the
                        // media corpus. Restrict that lane to the durable
                        // video allow-list; explicit image/audio modes still
                        // use their modality-specific allow-lists below.
                        let ordinary_shared_semantic = semantic_mode
                            && !image_mode
                            && !audio_mode
                            && q.vector_space.as_deref() == Some("shared");
                        // In a media-only query the durable allow-list is the
                        // complete candidate set for temporal windows.  Do
                        // not walk every CLIP/CLAP/media row and then filter
                        // it after the dot products: that made semantic
                        // video searches scale with the whole media corpus
                        // and was a noticeable source of the 3-5s delay.
                        // Shared mode still keeps the global pass for the
                        // primary store because it also contains text/page
                        // vectors; only the dedicated media store can be
                        // restricted safely here.
                        let media_only_video_semantic = q.vector_space.as_deref() == Some("media")
                            && semantic_mode
                            && !image_mode
                            && !audio_mode
                            && std::ptr::eq(store, self.media_vectors);
                        let implicit_media_video_semantic = q.vector_space.is_none()
                            && semantic_mode
                            && !image_mode
                            && !audio_mode
                            && std::ptr::eq(store, self.media_vectors);
                        if (ordinary_shared_semantic
                            || media_only_video_semantic
                            || implicit_media_video_semantic)
                            && std::ptr::eq(store, self.media_vectors)
                        {
                            let media_video_ids: HashSet<i64> = video_ids
                                .iter()
                                .copied()
                                .filter(|id| self.media_vectors.has(*id))
                                .collect();
                            store.search(&qvec, vector_limit, Some(&media_video_ids))
                        } else {
                            // Keep the global and video-priority top-k heaps
                            // in one pass for the primary store; a second
                            // full scan made ordinary searches noticeably
                            // slower as the video corpus grew.
                            store.search_with_priority_allowlist(&qvec, vector_limit, video_ids)
                        }
                    } else {
                        store.search(
                            &qvec,
                            vector_limit,
                            shared_image_ids.as_ref().or(shared_audio_ids.as_ref()),
                        )
                    };
                    let vector_ids: Vec<i64> = vector_hits.iter().map(|(id, _)| *id).collect();
                    let image_sources = self.db.doc_image_vector_sources_with_model(&vector_ids);
                    let text_pages = self.db.doc_vector_page_sources(&vector_ids);
                    let video_segments = self.db.video_segments_by_vector_ids(&vector_ids);
                    // Resolve source file rows once for this vector batch.
                    // `get_by_id` inside the hit loop was the remaining N+1
                    // query path after synthetic page/video IDs were batched.
                    // A single source file can contribute many page/image/
                    // video-window vectors.  Deduplicate the resolved file
                    // IDs before hitting SQLite; otherwise a 500-vector
                    // result page could bind the same file hundreds of times
                    // in the `IN (...)` query and spend most of its time
                    // parsing a redundant statement.  Keep insertion order
                    // stable so the row map remains deterministic.
                    let mut source_ids = Vec::with_capacity(vector_hits.len());
                    let mut source_id_seen = HashSet::with_capacity(vector_hits.len());
                    for (fid, _) in &vector_hits {
                        let source_id = if let Some((file_id, _, _)) = image_sources.get(fid) {
                            Some(*file_id)
                        } else if let Some((file_id, _, _)) = text_pages.get(fid) {
                            Some(*file_id)
                        } else if let Some((file_id, _, _, _)) = video_segments.get(fid) {
                            Some(*file_id)
                        } else if *fid >= 0 && !crate::ai::is_doc_page_vector_id(*fid) {
                            Some(*fid)
                        } else if crate::ai::video_segment_source(*fid).is_some() {
                            // Synthetic video IDs without a matching
                            // SQLite segment row are stale; the hit loop
                            // will discard them as well.
                            None
                        } else {
                            None
                        };
                        if let Some(source_id) = source_id {
                            if source_id_seen.insert(source_id) {
                                source_ids.push(source_id);
                            }
                        }
                    }
                    let source_rows: HashMap<i64, FileRow> = self
                        .db
                        .files_by_ids(&source_ids)
                        .into_iter()
                        .map(|row| (row.id, row))
                        .collect();
                    semantic_source_rows.extend(source_rows.iter().map(|(id, row)| (*id, row.clone())));
                    for (fid, sc) in vector_hits {
                        let mut page_no = None;
                        let mut video_location = None;
                        let image_record = image_sources.get(&fid);
                        if let (Some(expected), Some((_, _, actual))) =
                            (q.vector_model_key.as_deref(), image_record)
                        {
                            if !crate::db::image_model_key_compatible_with_legacy(
                                expected, actual, store.accepts_unmarked_vectors(),
                            ) {
                                continue;
                            }
                        }
                        let image_page = image_record.map(|(file_id, page, _)| (*file_id, *page));
                        let text_page = text_pages.get(&fid);
                        if image_mode && image_page.is_none() && text_page.is_some() {
                            // The shared document-page vector combines text
                            // and image evidence; only the dedicated image
                            // mapping qualifies for an image result.
                            continue;
                        }
                        let source_id = if let Some((file_id, page)) = image_page {
                            page_no = Some(page);
                            file_id
                        } else if let Some((file_id, page, _kind)) = text_page {
                            page_no = Some(*page);
                            *file_id
                        } else if let Some((file_id, start, end, subtitle)) = video_segments.get(&fid) {
                            video_location = Some((*start, *end, subtitle.clone()));
                            *file_id
                        } else if crate::ai::is_doc_page_vector_id(fid) {
                            // A stale synthetic vector without a DB mapping
                            // must never be interpreted as a file ID.
                            continue;
                        } else if crate::ai::video_segment_source(fid).is_some() {
                            // A synthetic video ID is valid only while its
                            // durable segment row exists.  Changed/deleted
                            // videos can leave an old vector slot behind until
                            // the next checkpoint; never expose that stale
                            // vector as a current file hit.
                            continue;
                        } else if fid < 0 {
                            continue;
                        } else {
                            fid
                        };
                        if page_no.is_none() && semantic_mode {
                            page_no = semantic_page_hints.get(&source_id).copied();
                        }
                        // The primary shared store also contains ordinary
                        // file-level text vectors.  Image mode should use
                        // its PDF page vectors (text fused with that page's
                        // rendered image), while standalone media remains
                        // available from the dedicated media/CLIP stores.
                        let source_row = source_rows.get(&source_id).cloned();
                        if image_mode {
                            // Only dedicated PDF image vectors, standalone
                            // image/video vectors, or a shared multimodal
                            // file-level image vector may enter this lane.
                            // Fused text page vectors are excluded.
                            let dedicated_pdf_image = image_page.is_some();
                            let image_kind = source_row.as_ref().is_some_and(|row| {
                                matches!(row.kind.as_str(), "image" | "video")
                            });
                            if !dedicated_pdf_image && page_no.is_none() && !image_kind {
                                continue;
                            }
                        } else if audio_mode {
                            // Audio searches may hit audio files, video
                            // segments, or a video's shared audio embedding.
                            let audio_kind = source_row.as_ref().is_some_and(|row| {
                                matches!(row.kind.as_str(), "audio" | "video")
                            });
                            if !audio_kind {
                                continue;
                            }
                        }
                        if let Some(allowed) = allowed_ref {
                            if !allowed.contains(&source_id) {
                                continue;
                            }
                        }
                        // Apply the same raw-cosine noise floor whether or
                        // not the candidate also has lexical evidence.
                        let is_video = video_location.is_some()
                            || source_row.as_ref().is_some_and(|row| row.kind.eq_ignore_ascii_case("video"));
                        let add = if is_video {
                            video_semantic_score(sc, self.cfg.semantic_floor, query_chars)
                        } else {
                            let semantic_floor = if image_mode {
                                self.cfg.semantic_floor.max(0.45)
                            } else {
                                self.cfg.semantic_floor
                            };
                            mode_semantic_score(sc, semantic_floor, query_chars, image_mode)
                        };
                        let Some(_add) = add else {
                            continue;
                        };
                        if let Some(page) = page_no {
                            if best_page_matches.get(&source_id).is_none_or(|(score, _)| sc > *score) {
                                best_page_matches.insert(source_id, (sc, page));
                            }
                        }
                        if let Some(location) = video_location.as_ref() {
                            let candidate = (
                                subtitle_cue_match_rank(&location.2, &text),
                                sc,
                                location.clone(),
                            );
                            if best_video_matches
                                .get(&source_id)
                                .is_none_or(|current| semantic_video_match_is_better(&candidate, current))
                            {
                                best_video_matches.insert(source_id, candidate);
                            }
                        }
                        if semantic_matches.get(&source_id).is_none_or(|current| sc > current.0) {
                            semantic_matches.insert(source_id, (sc, page_no, video_location));
                        }
                    }
                }
                for (source_id, (similarity, page_no, video_location)) in semantic_matches {
                    let page_no = best_page_matches
                        .get(&source_id)
                        .map(|(_, page)| *page)
                        .or(page_no);
                    let video_location = best_video_matches
                        .get(&source_id)
                        .map(|(_, _, location)| location.clone())
                        .or(video_location);
                    let is_video = video_location.is_some()
                        || semantic_source_rows.get(&source_id).is_some_and(|row| row.kind.eq_ignore_ascii_case("video"));
                    let add = if is_video {
                        video_semantic_score(similarity, self.cfg.semantic_floor, query_chars)
                    } else {
                        let semantic_floor = if image_mode {
                            self.cfg.semantic_floor.max(0.45)
                        } else {
                            self.cfg.semantic_floor
                        };
                        mode_semantic_score(similarity, semantic_floor, query_chars, image_mode)
                    };
                    let Some(add) = add else {
                        continue;
                    };
                    if let Some((_, score, reason)) = scored.get_mut(&source_id) {
                        *score = fuse_relevance(*score, Some(add), self.cfg.semantic_weight);
                        // A lexical/name match carries a stronger explanation
                        // than a later semantic pass. Only relabel the neutral
                        // browse seed; never overwrite an existing full-text
                        // or filename reason with a weaker semantic label.
                        if reason == "全部" {
                            *reason = if image_mode {
                                "图像相似"
                            } else if audio_mode {
                                "音频相似"
                            } else {
                                "语义相似"
                            }
                            .to_string();
                        }
                    } else if let Some(row) = semantic_source_rows.get(&source_id).cloned() {
                        let reason = if image_mode {
                            "图像相似"
                        } else if audio_mode {
                            "音频相似"
                        } else {
                            "语义相似"
                        };
                        scored.insert(source_id, (row, add, reason.to_string()));
                    }
                    if let Some(page) = page_no {
                        matched_pages.insert(source_id, page);
                    }
                    if let Some(location) = video_location {
                        // A subtitle full-text hit carries a stronger, exact
                        // cue than a semantic window match. Preserve it when
                        // later semantic lanes visit the same video.
                        matched_videos.entry(source_id).or_insert(location);
                    }
                }
            }
        }

        // 跨模态：文字 → 图片/声音空间。
        //
        // 这是「用一句话描述找图片/音频/视频」的实现：CLIP/CLAP 把图片、
        // 音频和文字编码到同一个 512 维空间，余弦相似度就是「这段话有多像
        // 这张图/这段声音」。和上面语义段的关键区别是跨空间 —— 上面比的是
        // 文本与文本，这里比的是文本与像素/波形。
        if (!explicit_mode || image_mode || audio_mode) && !low_text {
            self.cross_modal_segment(
                q,
                &rows,
                &mut scored,
                &mut matched_pages,
                &mut matched_videos,
            );
        }

        // 过滤阈值 + 排序
        let mut list: Vec<(FileRow, f32, String)> = scored
            .into_values()
            .filter(|(_, s, _)| query_chars == 0 || *s >= MIN_RESULT_SCORE)
            .collect();
        let sort = q.sort.clone().unwrap_or_else(|| "relevance".to_string());
        match sort.as_str() {
            "name" => list.sort_by(|a, b| {
                a.0.name
                    .to_lowercase()
                    .cmp(&b.0.name.to_lowercase())
                    .then_with(|| a.0.id.cmp(&b.0.id))
            }),
            "size" => list.sort_by(|a, b| a.0.size.cmp(&b.0.size).then_with(|| a.0.id.cmp(&b.0.id))),
            "mtime" => list.sort_by(|a, b| a.0.mtime.cmp(&b.0.mtime).then_with(|| a.0.id.cmp(&b.0.id))),
            "kind" => list.sort_by(|a, b| a.0.kind.cmp(&b.0.kind).then_with(|| a.0.id.cmp(&b.0.id))),
            _ => list.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.id.cmp(&b.0.id))
            }),
        }
        if q.desc && sort != "relevance" {
            list.reverse();
        }
        // Materialize only the requested result page before loading snippets.
        // The previous implementation performed one SQLite query per FTS hit
        // while constructing cards, which made a 500-result page needlessly
        // serialize hundreds of mutex acquisitions. Fetch all page bodies in
        // one bounded query instead.
        let page: Vec<(FileRow, f32, String)> = list
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect();
        let snippet_ids: Vec<i64> = page
            .iter()
            .filter(|(row, _, _)| fts_ids.contains(&row.id))
            .map(|(row, _, _)| row.id)
            .collect();
        let snippet_raw = self.db.doc_raw_by_ids(&snippet_ids);
        page.into_iter()
            .map(|(r, s, reason)| {
                let mut hit = Hit::from_row(&r, s, &reason);
                // The selected lane is also the user-facing explanation. It
                // keeps mixed legacy reasons (name/path/CLIP) from leaking
                // into the four-mode result cards.
                hit.reason = match q.search_mode.as_deref() {
                    Some(mode) if mode.eq_ignore_ascii_case("fulltext") => "全文匹配".to_string(),
                    Some(mode) if mode.eq_ignore_ascii_case("semantic") => "语义相似".to_string(),
                    Some(mode) if mode.eq_ignore_ascii_case("image") => "图像相似".to_string(),
                    Some(mode) if mode.eq_ignore_ascii_case("audio") => "音频相似".to_string(),
                    _ => hit.reason,
                };
                hit.page = matched_pages.get(&r.id).copied();
                if let Some((start, end, subtitle)) = matched_videos.get(&r.id) {
                    let (start_seconds, end_seconds, matched_subtitle) =
                        refine_video_location(*start, *end, subtitle, &text);
                    hit.start_seconds = Some(start_seconds);
                    hit.end_seconds = Some(end_seconds);
                    hit.subtitle = Some(matched_subtitle);
                }
                // 正文命中要能看见上下文，否则用户不知道「为什么这条算命中」。
                if let Some(raw) = snippet_raw.get(&r.id) {
                    hit.snippet = make_snippet(raw, &text, 60);
                }
                hit
            })
            .collect()
    }

    /// 跨模态检索段：把文字查询投到图片/声音空间里找最近邻。
    ///
    /// 与「语义相似」的区别在于跨的是模态，不是同一种文本的语义。
    fn cross_modal_segment(
        &self,
        q: &SearchQuery,
        rows: &[FileRow],
        scored: &mut HashMap<i64, (FileRow, f32, String)>,
        matched_pages: &mut HashMap<i64, i64>,
        matched_videos: &mut HashMap<i64, (i64, i64, String)>,
    ) {
        let filtered = has_filters(q);
        let allowed: HashSet<i64> = if filtered {
            rows.iter().map(|r| r.id).collect()
        } else {
            HashSet::new()
        };
        let by_id: HashMap<i64, &FileRow> = rows.iter().map(|r| (r.id, r)).collect();

        // 先各查一遍，拿两个空间的最高余弦做仲裁。
        //
        // 两个模型对「不相干的描述」都会给个不低的余弦（CLIP 尤其宽容），
        // 所以只靠绝对阈值永远会互相串味：「钢琴」会带出图片，「一只黑色的
        // 狗」会带出音频。这两个余弦不在同一量纲上（CLIP 0.3 就算沾边，
        // CLAP 0.3 已经很准），所以要先各自换算成最终分数再比。
        let image_mode = q.search_mode.as_deref().is_some_and(|mode| mode.eq_ignore_ascii_case("image"));
        let audio_mode = q.search_mode.as_deref().is_some_and(|mode| mode.eq_ignore_ascii_case("audio"));
        let clip_p = CrossParams {
            // CLIP 的余弦天生偏高：随机配对的图片/文字也在 0.2~0.3，
            // 所以 0.25 才是噪声线，0.45 已经是很强的匹配。
            // CLIP cosine values are not calibrated probabilities.  Values
            // around 0.25–0.35 are common for arbitrary images and used to
            // turn into visible 0.6–0.7 results.  Keep only a meaningful
            // contrastive match and map it into the same user-facing range.
            floor: 0.40,
            span: 0.20,
            base: 0.70,
            gain: 0.25,
            reason: if image_mode { "图像相似" } else { "文字描述匹配画面" },
        };
        let audio_p = CrossParams {
            // CLAP 的余弦绝对值低得多：真命中也只有 0.19~0.46，
            // 0.16 以下基本是「什么都沾一点」的幻觉。
            floor: 0.16,
            span: 0.30,
            base: 0.52,
            gain: 0.36,
            reason: if audio_mode { "音频相似" } else { "文字描述匹配声音" },
        };
        // Restrict synthetic PDF page IDs to the CLIP model that produced the
        // query. `vectors_clip` is a durable store and can contain pages
        // indexed by a previous CLIP model; searching its unrestricted top-k
        // would silently mix incompatible spaces and may crowd current pages
        // out before the metadata check below. Standalone image/video rows
        // are retained for backwards compatibility because their file-level
        // vectors do not have a model_key column yet.
        let clip_allowed_ids = q.clip_model_key.as_deref().map(|model_key| {
            let mut ids: HashSet<i64> = self
                .db
                .doc_image_vector_ids_for_model(model_key)
                .unwrap_or_default()
                .into_iter()
                .collect();
            ids.extend(
                rows.iter()
                    .filter(|row| matches!(row.kind.as_str(), "image" | "video"))
                    .map(|row| row.id),
            );
            ids
        });
        let clip_hits = q.clip_query.as_ref().map(|qv| {
            let mut v = qv.clone();
            normalize(&mut v);
            // 媒体空间里「视频」的向量也是从画面来的，所以可以一起查。
            self.clip_vectors
                .search(&v, limit_for_cross(q), clip_allowed_ids.as_ref())
        });
        let audio_hits = q.audio_query.as_ref().map(|qv| {
            let mut v = qv.clone();
            normalize(&mut v);
            self.audio_vectors.search(&v, limit_for_cross(q), None)
        });
        // 「突出度」= top1 减去中位数。两个模型对无关内容的基线差很多
        // （CLIP 随便配都 0.30，CLAP 只有 0.13），比绝对值等于比基线，
        // 比重心之上的抬升才是「这句话真在说这个东西」。
        let peak_of = |h: &Option<Vec<(i64, f32)>>, p: &CrossParams| {
            let Some(v) = h.as_ref().filter(|v| !v.is_empty()) else { return f32::MIN };
            let mut s: Vec<f32> = v.iter().map(|(_, x)| *x).collect();
            s.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            if s[0] < p.floor {
                return f32::MIN;
            }
            s[0] - s[s.len() / 2]
        };
        let (clip_peak, audio_peak) = (peak_of(&clip_hits, &clip_p), peak_of(&audio_hits, &audio_p));
        // 抬升不到 0.05 就说明第一名没从背景里跳出来，这个空间对这句话
        // 无话可说——实测「钢琴」的 CLIP 抬升只有 0.02，「一只黑色的狗」
        // 的 CLAP 抬升只有 0.018。
        const MIN_PEAK: f32 = 0.05;
        // 查询听起来就是在找声音（「钢琴」「鸟叫」「下雨的声音」）时，
        // CLAP 说了算，CLIP 一律让路，否则「钢琴」会带出几张图片。
        // 反过来，纯画面描述里 CLAP 只是瞎猜，要突出得多才准它插话——
        // 「峡湾和湖泊」的 CLAP 抬升也有 0.158，但 CLIP 更明确。
        let sound_intent = crate::ai::clap::sound_intent(q.text.trim());
        let use_audio = !image_mode
            && audio_peak >= MIN_PEAK
            && (sound_intent || audio_peak >= clip_peak * 2.0);
        let clip_top = clip_hits
            .as_ref()
            .and_then(|hits| hits.first())
            .map(|(_, score)| *score);
        let use_clip = should_use_clip_lane(
            image_mode,
            audio_mode,
            sound_intent,
            use_audio,
            clip_top,
            clip_peak,
            clip_p.floor,
            MIN_PEAK,
        );

        // 图片：clip_query 由调用方事先算好（编文本向量要跑一次模型）。
        if let Some(hits) = clip_hits.filter(|_| use_clip) {
            let hit_ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            let image_sources = self.db.doc_image_vector_sources_with_model(&hit_ids);
            let mut best_image_page: HashMap<i64, (f32, i64)> = HashMap::new();
            for (vector_id, similarity) in hits {
                if similarity < clip_p.floor {
                    continue;
                }

                // PDF image vectors have their own synthetic IDs and mapping
                // table. Resolve them before file filters/scoring so the UI
                // opens the exact source PDF page rather than dropping the
                // vector or treating its negative ID as a file ID.
                let image_source = image_sources
                    .get(&vector_id)
                    .map(|(file_id, page, _)| (*file_id, *page));
                if let Some(expected) = q.clip_model_key.as_deref() {
                    // A page mapping is the only CLIP vector kind with an
                    // explicit model identity. Empty/legacy mappings are
                    // rejected when a current key is available; otherwise a
                    // stale model could be presented as a valid image hit.
                    if let Some((_, _, actual)) = image_sources.get(&vector_id) {
                        if !crate::db::image_model_key_compatible(expected, actual) {
                            continue;
                        }
                    }
                }
                let (source_id, page_no) = image_source
                    .map(|(file_id, page)| (file_id, Some(page)))
                    .unwrap_or((vector_id, None));
                if source_id < 0 {
                    continue;
                }
                if filtered && !allowed.contains(&source_id) {
                    continue;
                }
                let row = by_id
                    .get(&source_id)
                    .map(|row| (*row).clone())
                    .or_else(|| self.db.get_by_id(source_id).ok().flatten());
                let Some(row) = row.filter(|row| !row.path.is_empty()) else {
                    continue;
                };

                if image_mode && page_no.is_none() {
                    let is_image_source = matches!(row.kind.as_str(), "image" | "video")
                        || (row.ext.eq_ignore_ascii_case("pdf")
                            && self
                                .db
                                .doc_image_vector_ids(source_id)
                                .map(|ids| ids.is_empty())
                                .unwrap_or(true));
                    if !is_image_source {
                        continue;
                    }
                }

                let relative = ((similarity - clip_p.floor) / clip_p.span).clamp(0.0, 1.0);
                let score = clip_p.base + clip_p.gain * relative;
                let should_replace = scored.get(&source_id).is_none_or(
                    |(_, current_score, current_reason)| {
                        should_replace_match(*current_score, current_reason, score, clip_p.reason)
                    },
                );
                if should_replace {
                    scored.insert(source_id, (row, score, clip_p.reason.to_string()));
                }
                if let Some(page) = page_no {
                    if best_image_page
                        .get(&source_id)
                        .is_none_or(|(best, _)| similarity > *best)
                    {
                        best_image_page.insert(source_id, (similarity, page));
                    }
                }
            }
            for (file_id, (_, page)) in best_image_page {
                matched_pages.insert(file_id, page);
            }
        }

        // 声音：CLAP 文本塔（中文查询已由 state/cli 映射成英文提示）。
        if let Some(hits) = audio_hits.filter(|_| use_audio) {
            score_cross_space(
                &hits,
                filtered,
                &allowed,
                &by_id,
                self.db,
                scored,
                audio_p,
                matched_pages,
                matched_videos,
            );
        }
    }

    /// 结构化候选集
    fn candidate_rows(&self, q: &SearchQuery, limit: usize) -> Vec<FileRow> {
        let mut sql = String::from(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
             FROM files WHERE missing=0",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !q.kinds.is_empty() {
            let ph = vec!["?"; q.kinds.len()].join(",");
            sql.push_str(&format!(" AND kind IN ({ph})"));
            for k in &q.kinds {
                args.push(k.clone().into());
            }
        }
        if !q.exts.is_empty() {
            let ph = vec!["?"; q.exts.len()].join(",");
            sql.push_str(&format!(" AND ext IN ({ph})"));
            for e in &q.exts {
                args.push(e.to_lowercase().trim_start_matches('.').to_string().into());
            }
        }
        if !q.roots.is_empty() {
            sql.push_str(" AND (");
            for (i, r) in q.roots.iter().enumerate() {
                if i > 0 {
                    sql.push_str(" OR ");
                }
                let param = args.len() + 1;
                sql.push_str(&format!(
                    "path = ?{param} COLLATE NOCASE OR substr(path,1,length(?{param})+1) = (?{param} || char(92)) COLLATE NOCASE"
                ));
                args.push(crate::util::norm_path(r).trim_end_matches('\\').to_string().into());
            }
            sql.push(')');
        }
        if !q.categories.is_empty() {
            let ph = vec!["?"; q.categories.len()].join(",");
            sql.push_str(&format!(" AND category IN ({ph})"));
            for c in &q.categories {
                args.push(c.clone().into());
            }
        }
        if let Some(v) = q.size_min {
            sql.push_str(" AND size >= ?");
            args.push(v.into());
        }
        if let Some(v) = q.size_max {
            sql.push_str(" AND size <= ?");
            args.push(v.into());
        }
        if let Some(v) = q.mtime_after {
            sql.push_str(" AND mtime >= ?");
            args.push(v.into());
        }
        if let Some(v) = q.mtime_before {
            sql.push_str(" AND mtime <= ?");
            args.push(v.into());
        }
        sql.push_str(&format!(" LIMIT {limit}"));
        self.db.query_rows(&sql, args)
    }

    pub fn facets(&self, q: &SearchQuery) -> HashMap<String, Vec<(String, i64)>> {
        let rows = self.candidate_rows(q, 100_000);
        let mut by_kind: HashMap<String, i64> = HashMap::new();
        let mut by_ext: HashMap<String, i64> = HashMap::new();
        let mut by_cat: HashMap<String, i64> = HashMap::new();
        for r in rows {
            *by_kind.entry(r.kind.clone()).or_insert(0) += 1;
            if !r.ext.is_empty() {
                *by_ext.entry(r.ext.clone()).or_insert(0) += 1;
            }
            if let Some(c) = r.category {
                *by_cat.entry(c).or_insert(0) += 1;
            }
        }
        let mut out = HashMap::new();
        out.insert("kinds".to_string(), sort_desc(by_kind));
        out.insert(
            "exts".to_string(),
            sort_desc(by_ext).into_iter().take(40).collect(),
        );
        out.insert("categories".to_string(), sort_desc(by_cat));
        out
    }
}

/// 是否带了结构化筛选条件（决定全文命中是否需要与候选集求交）。
fn has_filters(q: &SearchQuery) -> bool {
    !q.kinds.is_empty()
        || !q.exts.is_empty()
        || !q.roots.is_empty()
        || !q.categories.is_empty()
        || q.size_min.is_some()
        || q.size_max.is_some()
        || q.mtime_after.is_some()
        || q.mtime_before.is_some()
}

fn video_candidates_allowed(q: &SearchQuery, rows: &[FileRow]) -> bool {
    if q.kinds.iter().any(|kind| kind.eq_ignore_ascii_case("video")) {
        return true;
    }
    if !q.kinds.is_empty() {
        return false;
    }
    if has_filters(q) {
        return rows.iter().any(|row| row.kind.eq_ignore_ascii_case("video"));
    }
    true
}

/// Resolve persisted temporal windows for a semantic query without allowing a
/// stale model identity to hide otherwise valid video vectors. The exact model
/// key is preferred; an empty key means the caller has no durable identity
/// (old callers and restored indexes), while a non-empty key falls back to a
/// same-space/same-dimension scan when no durable model identity was supplied.
/// The vector store still verifies every returned synthetic ID.
fn video_ids_for_query(
    db: &Db,
    vector_space: &str,
    model_key: Option<&str>,
    vector_dim: usize,
) -> HashSet<i64> {
    let requested = model_key.unwrap_or_default().trim();
    let keyed = if requested.is_empty() {
        String::new()
    } else {
        crate::index::video_model_key(requested)
    };
    let ids = db
        .video_segment_vector_ids_for_compatible_model(vector_space, &keyed, vector_dim)
        .unwrap_or_default();
    ids.into_iter().collect()
}

/// 从正文里截一段包含查询词的上下文，两侧各留 `pad` 个字符。
fn make_snippet(raw: &str, query: &str, pad: usize) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    // 查询词可能被分词过，先用整串找，找不到再退回第一个 2-gram
    let pos = lower.find(&q).or_else(|| {
        let terms = crate::util::fts_query_terms(&q);
        terms.iter().filter_map(|t| lower.find(t)).min()
    });
    let Some(byte_pos) = pos else {
        let head: String = raw.chars().take(pad * 2).collect();
        return Some(clean_ws(&head));
    };
    let char_pos = lower[..byte_pos].chars().count();
    let start = char_pos.saturating_sub(pad);
    let end = (char_pos + q.chars().count() + pad).min(raw.chars().count());
    let body: String = raw
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    let mut s = clean_ws(&body);
    if start > 0 {
        s = format!("…{s}");
    }
    if end < raw.chars().count() {
        s.push('…');
    }
    Some(s)
}

fn clean_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sort_desc(m: HashMap<String, i64>) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

fn norm_path_lower(p: &str) -> String {
    p.to_lowercase().replace('/', "\\")
}

/// 跨模态打分的参数：两个模型的分数量纲差得很远，各给各的。
struct CrossParams<'a> {
    /// 低于这个余弦直接当噪声丢掉
    floor: f32,
    /// 从 floor 涨到「满分」需要多少余弦
    span: f32,
    /// 刚好卡在 floor 上的命中能拿到的分数
    base: f32,
    /// 余弦打满时再加多少
    gain: f32,
    reason: &'a str,
}

/// Retrieval scores come from differently calibrated lanes.  When a file is
/// returned by more than one lane, the visible explanation follows the
/// product precedence: full text, semantic, image, audio.
fn reason_priority(reason: &str) -> u8 {
    if reason.is_empty() || reason == "全部" {
        return 0;
    }
    if reason.contains("音频") || reason.contains("声音") || reason.eq_ignore_ascii_case("audio") {
        return 1;
    }
    if reason.contains("图像")
        || reason.contains("画面")
        || reason.contains("图片")
        || reason.eq_ignore_ascii_case("image")
    {
        return 2;
    }
    if reason.contains("语义") || reason.eq_ignore_ascii_case("semantic") {
        return 3;
    }
    if reason.contains("全文")
        || reason.contains("名称")
        || reason.contains("路径")
        || reason.contains("拼音")
        || reason.contains("模糊")
        || reason.eq_ignore_ascii_case("fulltext")
    {
        return 4;
    }
    0
}

fn should_replace_match(
    current_score: f32,
    current_reason: &str,
    candidate_score: f32,
    candidate_reason: &str,
) -> bool {
    let current_priority = reason_priority(current_reason);
    let candidate_priority = reason_priority(candidate_reason);
    candidate_priority > current_priority
        || (candidate_priority == current_priority && candidate_score > current_score)
}

fn should_use_clip_lane(
    image_mode: bool,
    audio_mode: bool,
    sound_intent: bool,
    use_audio: bool,
    clip_top: Option<f32>,
    clip_peak: f32,
    clip_floor: f32,
    min_peak: f32,
) -> bool {
    if audio_mode {
        return false;
    }
    if image_mode {
        // Explicit image searches should not need a top-vs-median score gap.
        // A small corpus or repeated PDF diagrams can otherwise hide valid hits.
        return clip_top.is_some_and(|score| score >= clip_floor);
    }
    clip_peak >= min_peak && !(sound_intent && use_audio)
}

/// 把跨模态近邻并进总分表。
///
/// 统一成「只加分不降分」：名称精确/前缀命中仍然是更强的信号，不能被
/// 一句描述覆盖掉用户的明确意图；反过来，跨模态命中也应该盖过路径匹配。
fn score_cross_space(
    hits: &[(i64, f32)],
    filtered: bool,
    allowed: &HashSet<i64>,
    by_id: &HashMap<i64, &FileRow>,
    db: &Db,
    scored: &mut HashMap<i64, (FileRow, f32, String)>,
    p: CrossParams,
    matched_pages: &mut HashMap<i64, i64>,
    matched_videos: &mut HashMap<i64, (i64, i64, String)>,
) {
    let vector_ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
    let image_sources = db.doc_image_vector_sources_with_model(&vector_ids);
    let video_segments = db.video_segments_by_vector_ids(&vector_ids);
    // Cross-modal candidates frequently run without structural filters, in
    // which case `by_id` is empty. Resolve all source files in one query
    // instead of calling `get_by_id` for every CLIP/CLAP hit.
    let source_ids: Vec<i64> = hits
        .iter()
        .filter_map(|(fid, _)| {
            if let Some((file_id, _, _)) = image_sources.get(fid) {
                Some(*file_id)
            } else if let Some((file_id, _, _, _)) = video_segments.get(fid) {
                Some(*file_id)
            } else if *fid >= 0 && !crate::ai::is_doc_image_vector_id(*fid) {
                Some(*fid)
            } else {
                crate::ai::video_segment_source(*fid)
            }
        })
        .collect();
    let source_rows: HashMap<i64, FileRow> = db
        .files_by_ids(&source_ids)
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    for &(fid, sc) in hits {
        let mut page_no = None;
        let mut video_location = None;
        let source_id = if let Some((file_id, page, _model)) = image_sources.get(&fid) {
            page_no = Some(*page);
            *file_id
        } else if let Some((file_id, start, end, subtitle)) = video_segments.get(&fid) {
            video_location = Some((*start, *end, subtitle.clone()));
            *file_id
        } else if crate::ai::is_doc_image_vector_id(fid) || fid < 0 {
            // Synthetic vectors without a live mapping are stale. Never
            // expose their internal ID as a file result.
            continue;
        } else {
            fid
        };
        if filtered && !allowed.contains(&source_id) {
            continue;
        }
        if sc < p.floor {
            continue;
        }
        let rel = ((sc - p.floor) / p.span).clamp(0.0, 1.0);
        let add = p.base + p.gain * rel;
        if let Some((_, score, reason)) = scored.get_mut(&source_id) {
            if should_replace_match(*score, reason, add, p.reason) {
                *score = add;
                *reason = p.reason.to_string();
            }
        } else if let Some(r) = by_id.get(&source_id) {
            scored.insert(source_id, ((*r).clone(), add, p.reason.to_string()));
        } else if let Some(r) = source_rows.get(&source_id) {
            scored.insert(source_id, (r.clone(), add, p.reason.to_string()));
        }
        if let Some(page) = page_no {
            matched_pages.insert(source_id, page);
        }
        if let Some(location) = video_location {
            // Keep an existing full-text/semantic cue when another cross-modal
            // lane reaches the same file later in the search pipeline.
            matched_videos.entry(source_id).or_insert(location);
        }
    }
}

/// 相似检索：用给定向量找最相近的文件（排除自身）。
///
/// 空间选择很关键：图片/视频优先用跨模态空间（CLIP 看的是「画面内容像不像」），
/// 音频优先用 CLAP（「听起来像不像」）；模型没装时才退回 hash 特征空间
/// （那比的是颜色分布/能量包络，粗但能用）。
pub fn similar(
    db: &Db,
    vectors: &VectorStore,
    media_vectors: &VectorStore,
    clip_vectors: &VectorStore,
    audio_vectors: &VectorStore,
    file_id: i64,
    kind_filter: Option<&str>,
    top: usize,
) -> Vec<Hit> {
    similar_scoped(
        db,
        vectors,
        media_vectors,
        clip_vectors,
        audio_vectors,
        file_id,
        kind_filter,
        top,
        None,
    )
}

/// Similar-file search scoped to active scan roots. `None` keeps the legacy
/// unrestricted behavior for callers such as isolated self-tests.
pub fn similar_scoped(
    db: &Db,
    vectors: &VectorStore,
    media_vectors: &VectorStore,
    clip_vectors: &VectorStore,
    audio_vectors: &VectorStore,
    file_id: i64,
    kind_filter: Option<&str>,
    top: usize,
    allowed_roots: Option<&[String]>,
) -> Vec<Hit> {
    let allowed_ids = match allowed_roots {
        Some(roots) => match db.file_ids_in_roots(roots) {
            Ok(ids) => Some(ids.into_iter().collect::<HashSet<_>>()),
            Err(_) => return Vec::new(),
        },
        None => None,
    };
    if allowed_ids.as_ref().is_some_and(|ids| !ids.contains(&file_id)) {
        return Vec::new();
    }
    let source = db.get_by_id(file_id).ok().flatten();
    let src_kind = source.as_ref().map(|r| r.kind.clone());
    let is_pdf = source.as_ref().map(|r| r.ext.eq_ignore_ascii_case("pdf")).unwrap_or(false);
    let spaces: [&VectorStore; 4] = [clip_vectors, audio_vectors, vectors, media_vectors];
    // 先按源文件类型挑首选空间，找不到再按顺序兜底 ——
    // 用户刚装好 CLIP 时，老文件只有 hash 向量，不能直接报「没有相似项」。
    let order: Vec<&VectorStore> = match (is_pdf, src_kind.as_deref()) {
        (true, _) | (false, Some("image" | "video")) => {
            vec![clip_vectors, vectors, media_vectors]
        }
        (false, Some("audio")) => vec![audio_vectors, media_vectors],
        _ => vec![vectors, media_vectors],
    };
    let mut store = None;
    let mut raw = Vec::new();
    for s in order.iter().chain(spaces.iter()) {
        if let Some(v) = s.get(file_id) {
            store = Some(*s);
            raw = v;
            break;
        }
    }
    let Some(store) = store else {
        return vec![];
    };
    let mut qvec = raw;
    normalize(&mut qvec);
    // 媒体空间里图片/音频/视频共用 256 维，但特征的物理含义完全不同
    // （颜色直方图 / 频谱 / 帧均值），跨类型比出来的分数毫无意义。
    // 所以默认只在「同一种类型」里找相似，显式传 kind_filter 时才覆盖。
    let want_kind = match kind_filter {
        Some(k) if k != "auto" && !k.is_empty() => Some(k.to_string()),
        _ => src_kind.clone(),
    };
    // Synthetic page/video IDs are not members of `allowed_ids`; resolve them
    // to their source file before applying the directory scope. Keep the best
    // location per file so one long document/video cannot occupy the gallery.
    let candidate_limit = top.saturating_mul(64).max(800).min(MAX_RESULT_LIMIT);
    let hits = store.search(&qvec, candidate_limit, None);
    let mut best_by_file: HashMap<i64, Hit> = HashMap::new();
    for (fid, sc) in hits {
        let mut page = None;
        let mut video = None;
        let source_id = if let Ok(Some((source, page_no, _))) = db.doc_vector_page_source(fid) {
            page = Some(page_no);
            source
        } else if let Ok(Some((source, start, end, subtitle))) = db.video_segment_by_vector_id(fid) {
            video = Some((start, end, subtitle));
            source
        } else if crate::ai::is_doc_page_vector_id(fid) {
            continue;
        } else if let Some(source) = crate::ai::video_segment_source(fid) {
            source
        } else if fid < 0 {
            continue;
        } else {
            fid
        };
        if allowed_ids.as_ref().is_some_and(|ids| !ids.contains(&source_id)) {
            continue;
        }
        if source_id == file_id {
            continue;
        }
        if let Ok(Some(r)) = db.get_by_id(source_id) {
            if r.path.is_empty() {
                continue;
            }
            // Hide weak semantic neighbors using the same noise floor as
            // normal search; do not treat 0.50 cosine as a confident match.
            let Some(sc) = semantic_score(sc, MIN_RESULT_SCORE, 3) else {
                continue;
            };
            if let Some(ref k) = want_kind {
                if &r.kind != k {
                    continue;
                }
            }
            let mut hit = Hit::from_row(&r, sc, "向量相似");
            hit.page = page;
            if let Some((start, end, subtitle)) = video {
                hit.start_seconds = Some(start as f64 / 1000.0);
                hit.end_seconds = Some(end as f64 / 1000.0);
                hit.subtitle = Some(subtitle);
            }
            if best_by_file
                .get(&source_id)
                .is_none_or(|current| hit.score > current.score)
            {
                best_by_file.insert(source_id, hit);
            }
        }
    }
    let mut out: Vec<Hit> = best_by_file.into_values().collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    out.truncate(top);
    let _ = now_ts();
    out
}

#[cfg(test)]
mod root_scope_tests {
    use super::{
        apply_enabled_root_scope, fuse_relevance, image_semantic_score, should_use_clip_lane,
        apply_fts_match, contextual_lexical_relevance_score, lexical_relevance_score,
        mode_semantic_score, parse_indexed_subtitle_cues,
        reason_priority, refine_video_location, semantic_video_match_is_better,
        semantic_score, video_semantic_score, should_replace_match, subtitle_cue_match_rank, SemanticVideoMatch,
        SearchEngine, SearchQuery, video_candidates_allowed,
    };

    #[test]
    fn video_subtitle_queries_refine_match_to_the_best_timestamped_cue() {
        let subtitle = "[00:28.500-00:31.200] Engine starts now [00:42.000-00:45.500] Water pump spins up";
        assert_eq!(parse_indexed_subtitle_cues(subtitle).len(), 2);
        assert_eq!(
            refine_video_location(0, 30_000, subtitle, "water pump"),
            (42.0, 45.5, "Water pump spins up".to_string())
        );
        assert_eq!(
            refine_video_location(0, 30_000, subtitle, "Engine starts"),
            (28.5, 31.2, "Engine starts now".to_string())
        );
    }

    #[test]
    fn video_subtitle_query_keeps_window_for_old_or_nonmatching_subtitles() {
        assert_eq!(
            refine_video_location(30_000, 60_000, "legacy subtitle", "engine"),
            (30.0, 60.0, "legacy subtitle".to_string())
        );
        assert_eq!(
            refine_video_location(
                30_000,
                60_000,
                "[00:31.000-00:32.000] Engine starts",
                "water pump"
            ),
            (30.0, 60.0, "[00:31.000-00:32.000] Engine starts".to_string())
        );
    }

    #[test]
    fn video_priority_scan_is_skipped_for_non_video_kind_filters() {
        let mut query = SearchQuery::default();
        query.kinds = vec!["document".to_string()];
        let document = crate::db::FileRow::new("C:\\docs\\manual.pdf");
        assert!(!video_candidates_allowed(&query, &[document]));

        query.kinds.clear();
        query.roots = vec!["C:\\docs".to_string()];
        assert!(!video_candidates_allowed(&query, &[crate::db::FileRow::new(
            "C:\\docs\\manual.pdf",
        )]));

        query.roots.clear();
        assert!(video_candidates_allowed(&query, &[]));
        query.kinds = vec!["video".to_string()];
        assert!(video_candidates_allowed(&query, &[]));
    }

    #[test]
    fn full_text_search_returns_video_and_exact_subtitle_timestamp() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow, VideoSegmentMeta};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut video = FileRow::new("C:\\media\\engine-training.mp4");
        video.kind = "video".into();
        let video_id = db.upsert_file(&video).unwrap();
        db.replace_video_segments(
            video_id,
            &[
                VideoSegmentMeta {
                    vector_id: -3_000_001,
                    file_id: video_id,
                    segment_no: 0,
                    start_ms: 0,
                    end_ms: 30_000,
                    subtitle: "[00:04.000-00:06.000] engine idles".into(),
                },
                VideoSegmentMeta {
                    vector_id: -3_000_002,
                    file_id: video_id,
                    segment_no: 1,
                    start_ms: 30_000,
                    end_ms: 60_000,
                    subtitle: "[00:41.500-00:45.250] engine starts flywheel rotation".into(),
                },
            ],
        )
        .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-fulltext-video-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };

        let query = SearchQuery {
            text: "engine starts flywheel".into(),
            search_mode: Some("fulltext".into()),
            limit: Some(10),
            ..Default::default()
        };
        let hits = engine.search(&query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, video_id);
        assert_eq!(hits[0].start_seconds, Some(41.5));
        assert_eq!(hits[0].end_seconds, Some(45.25));
        assert_eq!(
            hits[0].subtitle.as_deref(),
            Some("engine starts flywheel rotation")
        );

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_text_pdf_terms_on_different_pages_do_not_become_an_exact_hit() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut pdf = FileRow::new("C:\\docs\\split-terms.pdf");
        pdf.kind = "document".into();
        let file_id = db.upsert_file(&pdf).unwrap();
        let raw = "活塞\u{c}油漆";
        db.set_doc(
            file_id,
            &pdf.name,
            &crate::util::index_text(raw),
            raw,
        )
        .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-pdf-page-fts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };

        let query = SearchQuery {
            text: "活塞 油漆".into(),
            search_mode: Some("fulltext".into()),
            limit: Some(10),
            ..Default::default()
        };
        let hits = engine.search(&query);
        // Each page contains only one query component.  The page-aware
        // contextual scorer must reject this as an ambiguous partial hit;
        // otherwise a multi-page PDF would be reported as a full-text match
        // even though no single page contains the requested concept.
        assert!(hits.is_empty());

        let raw_same_page = "活塞油漆";
        db.set_doc(
            file_id,
            &pdf.name,
            &crate::util::index_text(raw_same_page),
            raw_same_page,
        )
        .unwrap();
        let exact_hits = engine.search(&query);
        assert_eq!(exact_hits.len(), 1);
        assert_eq!(exact_hits[0].score, 1.0);
        assert_eq!(exact_hits[0].page, Some(1));

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_fulltext_with_index_root_and_type_filters_keeps_matching_rows() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow};
        use crate::vector::VectorStore;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut included = FileRow::new("E:\\项目\\CN\\engine.pdf");
        included.kind = "document".into();
        let id = db.upsert_file(&included).unwrap();
        let raw = "前言\u{c}发动机的活塞环检查";
        db.set_doc(id, &included.name, &crate::util::index_text(raw), raw).unwrap();
        let mut excluded = FileRow::new("E:\\其他\\another.pdf");
        excluded.kind = "document".into();
        let other_id = db.upsert_file(&excluded).unwrap();
        db.set_doc(other_id, &excluded.name, &crate::util::index_text(raw), raw).unwrap();
        let root = std::env::temp_dir().join(format!("deepaisee-scoped-fts-{}-{}",std::process::id(),crate::util::now_ts()));
        let vectors = VectorStore::open(&root.join("vectors"), 768).unwrap();
        let media = VectorStore::open(&root.join("media"), 768).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 768).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 768).unwrap();
        let cfg = SearchConfig::default();
        let engine = SearchEngine { db: &db, vectors: &vectors, media_vectors: &media, clip_vectors: &clip, audio_vectors: &audio, cfg: &cfg };
        let mut query = SearchQuery {
            text: "活塞环".into(), search_mode: Some("fulltext".into()),
            roots: vec!["E:\\项目\\CN".into()], ..Default::default()
        };
        for kinds in [vec![], vec!["document".into()]] {
            query.kinds = kinds;
            let hits = engine.search(&query);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, id);
            assert_eq!(hits[0].page, Some(2));
            assert_eq!(hits[0].score, 1.0);
        }
        query.kinds = vec!["image".into()];
        assert!(engine.search(&query).is_empty());
        drop(engine);
        drop((vectors, media, clip, audio));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiword_fulltext_suppresses_single_component_hits_and_marks_phrase_exactly() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow};
        use crate::vector::VectorStore;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut exact = FileRow::new("C:\\docs\\engine-piston-exact.pdf");
        exact.kind = "document".into();
        let exact_id = db.upsert_file(&exact).unwrap();
        let exact_body = "发动机活塞检查";
        db.set_doc(exact_id, &exact.name, &crate::util::index_text(exact_body), exact_body).unwrap();

        let mut contextual = FileRow::new("C:\\docs\\engine-piston-context.pdf");
        contextual.kind = "document".into();
        let contextual_id = db.upsert_file(&contextual).unwrap();
        let contextual_body = "发动机系统完成检查，随后测量活塞间隙";
        db.set_doc(
            contextual_id,
            &contextual.name,
            &crate::util::index_text(contextual_body),
            contextual_body,
        ).unwrap();

        let mut engine_only = FileRow::new("C:\\docs\\engine-only.pdf");
        engine_only.kind = "document".into();
        let engine_only_id = db.upsert_file(&engine_only).unwrap();
        let engine_only_body = "发动机控制系统检查";
        db.set_doc(
            engine_only_id,
            &engine_only.name,
            &crate::util::index_text(engine_only_body),
            engine_only_body,
        ).unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-contextual-fts-{}-{}",
            std::process::id(),
            crate::util::now_ts()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        let query = SearchQuery {
            text: "发动机活塞".into(),
            search_mode: Some("fulltext".into()),
            limit: Some(10),
            ..Default::default()
        };
        let hits = engine.search(&query);
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), vec![exact_id, contextual_id]);
        assert_eq!(hits[0].score, 1.0);
        assert_eq!(hits[1].score, 0.70);

        // Legacy callers without an explicit lane use the default hybrid
        // path. Its fast FTS stage must apply the same context gate before
        // semantic recall, rather than reintroducing single-component hits.
        let hybrid_hits = engine.search(&SearchQuery {
            search_mode: None,
            ..query.clone()
        });
        assert_eq!(
            hybrid_hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            vec![exact_id, contextual_id]
        );
        assert!(!hits.iter().any(|hit| hit.id == engine_only_id));

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn semantic_video_search_prefers_a_query_matching_cue_across_windows() {
        let high_score_no_cue: SemanticVideoMatch = (
            subtitle_cue_match_rank(
                "[00:05.000-00:07.000] Engine idles smoothly",
                "water pump",
            ),
            0.94,
            (0, 30_000, "[00:05.000-00:07.000] Engine idles smoothly".into()),
        );
        let lower_score_matching_cue: SemanticVideoMatch = (
            subtitle_cue_match_rank(
                "[01:12.000-01:15.000] Water pump starts rotating",
                "water pump",
            ),
            0.68,
            (60_000, 90_000, "[01:12.000-01:15.000] Water pump starts rotating".into()),
        );

        assert!(semantic_video_match_is_better(
            &lower_score_matching_cue,
            &high_score_no_cue
        ));
        let (start, end, subtitle) = refine_video_location(
            lower_score_matching_cue.2 .0,
            lower_score_matching_cue.2 .1,
            &lower_score_matching_cue.2 .2,
            "water pump",
        );
        assert_eq!((start, end), (72.0, 75.0));
        assert_eq!(subtitle, "Water pump starts rotating");
    }

    #[test]
    fn semantic_video_search_falls_back_to_highest_vector_window_without_cue_match() {
        let lower_score: SemanticVideoMatch = (
            subtitle_cue_match_rank("[01:12.000-01:15.000] Pump starts rotating", "crankshaft"),
            0.68,
            (60_000, 90_000, "[01:12.000-01:15.000] Pump starts rotating".into()),
        );
        let higher_score: SemanticVideoMatch = (
            subtitle_cue_match_rank("[00:05.000-00:07.000] Engine idles smoothly", "crankshaft"),
            0.94,
            (0, 30_000, "[00:05.000-00:07.000] Engine idles smoothly".into()),
        );

        assert!(!semantic_video_match_is_better(&lower_score, &higher_score));
        assert!(semantic_video_match_is_better(&higher_score, &lower_score));
        assert_eq!(lower_score.0, None);
        assert_eq!(higher_score.0, None);
    }

    fn query_roots(requested: &[&str], configured: &[&str], disabled: &[&str]) -> Vec<String> {
        let mut query = SearchQuery::default();
        query.roots = requested.iter().map(|root| (*root).to_string()).collect();
        let configured: Vec<String> = configured.iter().map(|root| (*root).to_string()).collect();
        let disabled: Vec<String> = disabled.iter().map(|root| (*root).to_string()).collect();
        apply_enabled_root_scope(&mut query, &configured, &disabled);
        query.roots
    }

    #[test]
    fn defaults_to_enabled_roots_and_rejects_disabled_selection() {
        assert_eq!(
            query_roots(&[], &["D:\\Docs", "D:\\Media"], &["D:\\Docs"]),
            vec!["D:\\Media"]
        );
        assert_eq!(
            query_roots(&["D:\\Docs"], &["D:\\Docs", "D:\\Media"], &["D:\\Docs"]),
            vec!["__deepdark_oracle_no_enabled_index_root__"]
        );
    }

    #[test]
    fn preserves_nested_selection_but_clamps_parent_selection() {
        assert_eq!(
            query_roots(&["D:\\Media\\Photos"], &["D:\\Media"], &[]),
            vec!["D:\\Media\\Photos"]
        );
        assert_eq!(
            query_roots(&["D:\\"], &["D:\\Media"], &[]),
            vec!["D:\\Media"]
        );
    }

    #[test]
    fn match_reason_priority_prefers_text_over_cross_modal_lanes() {
        assert_eq!(reason_priority("全文内容匹配"), 4);
        assert_eq!(reason_priority("语义相似"), 3);
        assert_eq!(reason_priority("图像相似"), 2);
        assert_eq!(reason_priority("音频相似"), 1);
        // A weaker textual score still keeps the textual explanation.  Scores
        // from CLIP/CLAP are not comparable to FTS scores.
        assert!(!should_replace_match(0.55, "全文内容匹配", 0.95, "图像相似"));
        assert!(should_replace_match(0.55, "图像相似", 0.56, "语义相似"));
        assert!(should_replace_match(0.55, "语义相似", 0.80, "语义相似"));
    }

    #[test]
    fn semantic_scores_report_cosine_and_filter_ambiguous_short_queries() {
        assert_eq!(semantic_score(0.39, 0.45, 3), None);
        assert_eq!(semantic_score(0.53, 0.45, 3), None);
        assert_eq!(semantic_score(0.53, 0.45, 1), None);
        assert_eq!(semantic_score(0.61, 0.45, 1), None);
        assert_eq!(semantic_score(0.70, 0.45, 1), None);
        assert_eq!(semantic_score(0.72, 0.45, 1), None);
        assert!(semantic_score(0.78, 0.45, 1).is_some());
        // Regression: weak two-character CJK neighbors (e.g. 活塞 → 油漆)
        // must be discarded even if the configured general floor is lower.
        assert_eq!(semantic_score(0.69, 0.45, 2), None);
        assert_eq!(semantic_score(0.73, 0.45, 2), None);
        assert!(semantic_score(0.78, 0.45, 2).is_some());
        assert_eq!(semantic_score(0.61, 0.45, 3), None);
        assert_eq!(semantic_score(0.62, 0.45, 3), None);
        assert_eq!(semantic_score(0.68, 0.45, 3), Some(0.68));
        assert_eq!(semantic_score(0.70, 0.45, 3), Some(0.70));
        assert!((semantic_score(0.80, 0.45, 3).unwrap() - 0.833_333_3).abs() < 1e-6);
        assert!((semantic_score(0.85, 0.45, 3).unwrap() - 0.90).abs() < 1e-6);
        assert!(semantic_score(0.90, 0.45, 3).unwrap() > 0.90);
    }

    #[test]
    fn video_semantic_score_requires_relevance_even_with_a_timestamp() {
        for query_chars in [1, 2, 3, 10] {
            for noise in [0.39, 0.45, 0.55, 0.66, 0.67, f32::NAN, f32::INFINITY] {
                assert_eq!(video_semantic_score(noise, 0.45, query_chars), None);
            }
        }
        assert_eq!(video_semantic_score(0.73, 0.45, 2), None);
        assert!(video_semantic_score(0.78, 0.45, 2).is_some());
        assert!(video_semantic_score(0.70, 0.45, 3).is_some());
        assert_eq!(video_semantic_score(0.78, 0.85, 3), None);
    }

    #[test]
    fn image_mode_uses_cross_modal_floor_for_short_chinese_queries() {
        assert_eq!(mode_semantic_score(0.44, 0.45, 2, true), None);
        assert!(mode_semantic_score(0.50, 0.45, 2, true).is_some());
        assert_eq!(mode_semantic_score(0.50, 0.45, 2, false), None);
        assert!(image_semantic_score(0.82, 0.45).unwrap() > 0.90);
    }

    #[test]
    fn explicit_image_search_does_not_require_a_relative_clip_score_gap() {
        assert!(should_use_clip_lane(
            true, false, false, false, Some(0.52), 0.01, 0.40, 0.05
        ));
        assert!(!should_use_clip_lane(
            true, false, false, false, Some(0.39), 0.20, 0.40, 0.05
        ));
        assert!(!should_use_clip_lane(
            false, false, false, false, Some(0.52), 0.01, 0.40, 0.05
        ));
        assert!(!should_use_clip_lane(
            true, true, false, false, Some(0.52), 0.20, 0.40, 0.05
        ));
    }

    #[test]
    fn clip_image_search_filters_pdf_pages_from_other_models_before_top_k() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut file = FileRow::new("C:\\docs\\diagram.pdf");
        file.kind = "document".into();
        let file_id = db.upsert_file(&file).unwrap();
        let stale_id = crate::ai::doc_image_vector_id(file_id, 1);
        let current_id = crate::ai::doc_image_vector_id(file_id, 2);
        let current_key = "clip:new-model|2";
        db.replace_doc_image_vectors(
            file_id,
            &[
                (stale_id, 1, "stale-hash".into(), "clip:old-model|2".into()),
                (current_id, 2, "current-hash".into(), current_key.into()),
            ],
        )
        .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-clip-model-filter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        // The stale model would win an unrestricted top-k scan. Filtering by
        // model key must happen inside VectorStore::search, before it chooses
        // the nearest page vector.
        clip.add(stale_id, &[0.99, 0.01]);
        clip.add(current_id, &[0.80, 0.60]);
        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        let query = SearchQuery {
            text: "diagram".into(),
            search_mode: Some("image".into()),
            clip_query: Some(vec![1.0, 0.0]),
            clip_model_key: Some(current_key.into()),
            limit: Some(10),
            ..Default::default()
        };

        let hits = engine.search(&query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, file_id);
        assert_eq!(hits[0].page, Some(2));

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_image_and_audio_search_without_space_do_not_use_primary_text_vectors() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut image = FileRow::new("C:\\media\\image.png");
        image.kind = "image".into();
        let image_id = db.upsert_file(&image).unwrap();
        let mut audio_file = FileRow::new("C:\\media\\audio.wav");
        audio_file.kind = "audio".into();
        let audio_id = db.upsert_file(&audio_file).unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-modality-space-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        // These are file-level primary embeddings, not image/audio embeddings.
        vectors.add(image_id, &[1.0, 0.0]);
        vectors.add(audio_id, &[1.0, 0.0]);

        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        for (mode, expected_id) in [("image", image_id), ("audio", audio_id)] {
            for vector_space in [None, Some("shared")] {
                let query = SearchQuery {
                    text: "x".into(),
                    search_mode: Some(mode.into()),
                    query_vector: Some(vec![1.0, 0.0]),
                    // A space label without a stable model key still cannot
                    // prove that persisted modality vectors share this query's
                    // embedding space.
                    vector_space: vector_space.map(str::to_string),
                    limit: Some(10),
                    ..Default::default()
                };
                assert!(
                    engine.search(&query).is_empty(),
                    "{mode} mode with space {vector_space:?} must not compare against primary text vector for {expected_id}"
                );
            }
        }

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_video_noise_is_rejected_in_every_lane_and_vector_store() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow, VideoSegmentMeta};
        use crate::vector::VectorStore;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let root = std::env::temp_dir().join(format!(
            "deepdark-video-noise-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        let model_key = "shared:video-noise-regression|2";
        let video_key = crate::index::video_model_key(model_key);
        let mut strong_ids = std::collections::HashSet::new();
        let mut medium_ids = std::collections::HashSet::new();
        for (space, store) in [("primary", &vectors), ("media", &media)] {
            // Reproduce the five always-visible weak windows, plus a weak
            // legacy file-level vector without a timestamp. Include stronger
            // controls so this cannot pass by disabling video retrieval.
            for (n, cosine) in [0.66f32, 0.665, 0.67, 0.662, 0.668, 0.66, 0.73, 0.90].into_iter().enumerate() {
                let mut video = FileRow::new(&format!("C:\\media\\clip-{space}-{n}.mp4"));
                video.kind = "video".into();
                let id = db.upsert_file(&video).unwrap();
                let embedding = [cosine, (1.0 - cosine * cosine).sqrt()];
                if n == 5 {
                    store.add(id, &embedding);
                } else {
                    let vector_id = crate::ai::video_segment_id(id, 0);
                    db.upsert_video_segment_embedding(&VideoSegmentMeta {
                        vector_id, file_id: id, segment_no: 0,
                        start_ms: 30_000, end_ms: 60_000, subtitle: String::new(),
                    }, space, &video_key, &embedding).unwrap();
                    store.add(vector_id, &embedding);
                }
                if n == 6 { medium_ids.insert(id); }
                if n == 7 { strong_ids.insert(id); }
            }
        }
        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db, vectors: &vectors, media_vectors: &media,
            clip_vectors: &clip, audio_vectors: &audio, cfg: &cfg,
        };
        for space in ["shared", "media"] {
            for mode in ["semantic", "image", "audio", "hybrid"] {
                for text in ["猫猫", "完全无关的关键词"] {
                    let hits = engine.search(&SearchQuery {
                        text: text.into(), search_mode: Some(mode.into()),
                        use_semantic: Some(true), cross_modal: Some(false),
                        query_vector: Some(vec![1.0, 0.0]),
                        vector_space: Some(space.into()), vector_model_key: Some(model_key.into()),
                        limit: Some(100), ..Default::default()
                    });
                    assert!(!hits.is_empty(), "{space}/{mode}: strong video matches must survive");
                    for hit in &hits {
                        assert!(strong_ids.contains(&hit.id)
                            || (text.chars().count() > 2 && medium_ids.contains(&hit.id)),
                            "{space}/{mode}/{text}: weak video {} leaked at {}", hit.id, hit.score);
                        assert_eq!(hit.start_seconds, Some(30.0));
                        assert_eq!(hit.end_seconds, Some(60.0));
                    }
                }
            }
        }
        drop(engine);
        drop((vectors, media, clip, audio));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_image_search_resolves_video_window_and_preserves_time_range() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow, VideoSegmentMeta};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut video = FileRow::new("C:\\media\\engine.mp4");
        video.kind = "video".into();
        let video_id = db.upsert_file(&video).unwrap();
        let model_key = "gemma:video-image|768|280";
        let video_key = crate::index::video_model_key(model_key);
        let valid_id = crate::ai::video_segment_id(video_id, 4);
        let valid = VideoSegmentMeta {
            vector_id: valid_id,
            file_id: video_id,
            segment_no: 4,
            start_ms: 120_000,
            end_ms: 150_000,
            subtitle: "engine starts".into(),
        };
        db.upsert_video_segment_embedding(&valid, "primary", &video_key, &[0.82, 0.572_363_5])
            .unwrap();
        let stale_id = crate::ai::video_segment_id(video_id, 5);
        let stale = VideoSegmentMeta {
            vector_id: stale_id,
            file_id: video_id,
            segment_no: 5,
            start_ms: 150_000,
            end_ms: 180_000,
            subtitle: "old model frame".into(),
        };
        db.upsert_video_segment_embedding(&stale, "primary", "old:encoder|768", &[0.999, 0.045])
            .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-shared-image-video-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        vectors.add(valid_id, &[0.82, 0.572_363_5]);
        vectors.add(stale_id, &[0.999, 0.045]);

        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        let query = SearchQuery {
            text: "engine starting".into(),
            search_mode: Some("image".into()),
            query_vector: Some(vec![1.0, 0.0]),
            vector_space: Some("shared".into()),
            vector_model_key: Some(model_key.into()),
            limit: Some(10),
            ..Default::default()
        };
        let hits = engine.search(&query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, video_id);
        assert_eq!(hits[0].start_seconds, Some(120.0));
        assert_eq!(hits[0].end_seconds, Some(150.0));
        assert_eq!(hits[0].subtitle.as_deref(), Some("engine starts"));
        assert_eq!(hits[0].reason, "图像相似");

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn media_space_semantic_search_resolves_fallback_video_window() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow, VideoSegmentMeta};
        use crate::vector::VectorStore;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut video = FileRow::new("C:\\media\\fallback-video.mp4");
        video.kind = "video".into();
        let video_id = db.upsert_file(&video).unwrap();
        let model_key = "hash:media-fallback|256|280";
        let video_key = crate::index::video_model_key(model_key);
        let vector_id = crate::ai::video_segment_id(video_id, 2);
        db.upsert_video_segment_embedding(
            &VideoSegmentMeta {
                vector_id,
                file_id: video_id,
                segment_no: 2,
                start_ms: 60_000,
                end_ms: 90_000,
                subtitle: "pump starts".into(),
            },
            "media",
            &video_key,
            &[0.96, 0.28],
        )
        .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-media-video-semantic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        media.add(vector_id, &[0.96, 0.28]);

        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        let query = SearchQuery {
            text: "pump starts".into(),
            search_mode: Some("semantic".into()),
            query_vector: Some(vec![1.0, 0.0]),
            vector_space: Some("media".into()),
            vector_model_key: Some(model_key.into()),
            limit: Some(10),
            ..Default::default()
        };
        let hits = engine.search(&query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, video_id);
        assert_eq!(hits[0].start_seconds, Some(60.0));
        assert_eq!(hits[0].end_seconds, Some(90.0));
        assert_eq!(hits[0].subtitle.as_deref(), Some("pump starts"));
        assert_eq!(hits[0].reason, "语义相似");

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_audio_search_merges_primary_audio_and_current_video_windows_with_clap() {
        use crate::config::SearchConfig;
        use crate::db::{Db, FileRow, VideoSegmentMeta};
        use crate::vector::VectorStore;
        use std::collections::HashSet;
        use std::path::Path;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut shared_audio = FileRow::new("C:\\media\\shared-audio.wav");
        shared_audio.kind = "audio".into();
        let shared_audio_id = db.upsert_file(&shared_audio).unwrap();
        let mut clap_only_audio = FileRow::new("C:\\media\\clap-only.wav");
        clap_only_audio.kind = "audio".into();
        let clap_only_audio_id = db.upsert_file(&clap_only_audio).unwrap();
        let mut video = FileRow::new("C:\\media\\engine-video.mp4");
        video.kind = "video".into();
        let video_id = db.upsert_file(&video).unwrap();
        let mut document = FileRow::new("C:\\docs\\unrelated.txt");
        document.kind = "document".into();
        let document_id = db.upsert_file(&document).unwrap();

        let current_key = "shared:current-encoder|2";
        let current_video_key = crate::index::video_model_key(current_key);
        let current_segment_id = crate::ai::video_segment_id(video_id, 0);
        let current_segment = VideoSegmentMeta {
            vector_id: current_segment_id,
            file_id: video_id,
            segment_no: 0,
            start_ms: 30_000,
            end_ms: 60_000,
            subtitle: "engine starts".into(),
        };
        db.upsert_video_segment_embedding(
            &current_segment,
            "primary",
            &current_video_key,
            &[0.90, 0.435_889_9],
        )
        .unwrap();
        let stale_segment_id = crate::ai::video_segment_id(video_id, 1);
        let stale_segment = VideoSegmentMeta {
            vector_id: stale_segment_id,
            file_id: video_id,
            segment_no: 1,
            start_ms: 60_000,
            end_ms: 90_000,
            subtitle: "stale encoder window".into(),
        };
        db.upsert_video_segment_embedding(
            &stale_segment,
            "primary",
            "shared:previous-encoder|2",
            &[0.999, 0.045],
        )
        .unwrap();
        let wrong_dim_segment_id = crate::ai::video_segment_id(video_id, 2);
        let wrong_dim_segment = VideoSegmentMeta {
            vector_id: wrong_dim_segment_id,
            file_id: video_id,
            segment_no: 2,
            start_ms: 90_000,
            end_ms: 120_000,
            subtitle: "wrong dimension window".into(),
        };
        // Same model key is insufficient when an index was created with a
        // different configured vector dimension. This stronger vector must
        // be rejected before it can win top-k or overwrite the timestamp.
        db.upsert_video_segment_embedding(
            &wrong_dim_segment,
            "primary",
            &current_video_key,
            &[0.999, 0.045, 0.0],
        )
        .unwrap();

        let root = std::env::temp_dir().join(format!(
            "deepdark-shared-audio-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let vectors = VectorStore::open(&root.join("text"), 2).unwrap();
        let media = VectorStore::open(&root.join("media"), 2).unwrap();
        let clip = VectorStore::open(&root.join("clip"), 2).unwrap();
        let audio = VectorStore::open(&root.join("audio"), 2).unwrap();
        vectors.add(shared_audio_id, &[0.95, 0.312_249_9]);
        vectors.add(current_segment_id, &[0.90, 0.435_889_9]);
        // These stronger text and stale-model candidates would win an
        // unrestricted top-k scan, but neither belongs in current audio mode.
        vectors.add(document_id, &[0.999, 0.045]);
        vectors.add(stale_segment_id, &[0.999, 0.045]);
        vectors.add(wrong_dim_segment_id, &[0.999, 0.045]);
        // CLAP remains an independent audio lane and can return a file that
        // has no shared-space vector yet.
        audio.add(shared_audio_id, &[0.88, 0.475]);
        audio.add(clap_only_audio_id, &[0.98, 0.199]);

        let cfg = SearchConfig::default();
        let engine = SearchEngine {
            db: &db,
            vectors: &vectors,
            media_vectors: &media,
            clip_vectors: &clip,
            audio_vectors: &audio,
            cfg: &cfg,
        };
        let query = SearchQuery {
            text: "engine sound".into(),
            search_mode: Some("audio".into()),
            query_vector: Some(vec![1.0, 0.0]),
            vector_space: Some("shared".into()),
            vector_model_key: Some(current_key.into()),
            audio_query: Some(vec![1.0, 0.0]),
            limit: Some(10),
            ..Default::default()
        };

        let hits = engine.search(&query);
        let hit_ids: HashSet<i64> = hits.iter().map(|hit| hit.id).collect();
        assert!(hit_ids.contains(&shared_audio_id), "shared audio embedding should be searchable");
        assert!(hit_ids.contains(&clap_only_audio_id), "CLAP-only fallback should remain searchable");
        assert!(hit_ids.contains(&video_id), "current shared video window should resolve to its source file");
        assert!(!hit_ids.contains(&document_id), "text vectors must not leak into audio results");
        let video_hit = hits.iter().find(|hit| hit.id == video_id).unwrap();
        assert_eq!(video_hit.start_seconds, Some(30.0));
        assert_eq!(video_hit.end_seconds, Some(60.0));
        assert_eq!(video_hit.subtitle.as_deref(), Some("engine starts"));
        assert_eq!(video_hit.reason, "音频相似");

        drop(engine);
        drop(db);
        drop(vectors);
        drop(media);
        drop(clip);
        drop(audio);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lexical_phrase_and_term_coverage_have_stable_relevance_scores() {
        assert_eq!(lexical_relevance_score(true, 0, 3), Some(1.0));
        assert!((lexical_relevance_score(false, 3, 3).unwrap() - 0.88).abs() < 1e-6);
        assert!((lexical_relevance_score(false, 1, 2).unwrap() - 0.70).abs() < 1e-6);
        assert!((lexical_relevance_score(false, 2, 3).unwrap() - (0.52 + 0.36 * (2.0 / 3.0))).abs() < 1e-6);
        assert_eq!(lexical_relevance_score(false, 0, 3), None);
        assert_eq!(fuse_relevance(1.0, Some(0.99), 0.55), 1.0);
        assert_eq!(fuse_relevance(0.93, Some(0.8), 0.55), 0.93);
        assert!(fuse_relevance(0.7, Some(0.9), 0.55) > 0.7);
    }

    #[test]
    fn full_text_body_score_does_not_inherit_unrelated_seed() {
        let mut score = 0.75;
        let mut reason = "模糊匹配".to_string();
        apply_fts_match(&mut score, &mut reason, 0.70);
        assert!((score - 0.70).abs() < 1e-6);
        assert_eq!(reason, "全文内容匹配");

        // A filename exact match remains the stronger deterministic signal;
        // a weaker body hit must not lower it or relabel it.
        let mut name_score = 0.90;
        let mut name_reason = "名称精确匹配".to_string();
        apply_fts_match(&mut name_score, &mut name_reason, 0.70);
        assert!((name_score - 0.90).abs() < 1e-6);
        assert_eq!(name_reason, "名称精确匹配");
    }

    #[test]
    fn contextual_fulltext_requires_most_query_terms_on_one_page() {
        assert_eq!(contextual_lexical_relevance_score(true, 1, 4), Some(1.0));
        assert_eq!(contextual_lexical_relevance_score(false, 3, 4), Some(0.70));
        assert_eq!(contextual_lexical_relevance_score(false, 4, 4), Some(0.70));
        assert!(contextual_lexical_relevance_score(false, 2, 4).is_none());
        assert!(contextual_lexical_relevance_score(false, 1, 2).is_none());
        assert_eq!(contextual_lexical_relevance_score(true, 1, 4), Some(1.0));
    }
}
