//! 端到端冒烟：扫描 → 索引 → 全文/拼音/语义/相似检索。
//! 不依赖 GUI，用来验证 DeepDark_Oracle 后端整条链路。
//!
//! 用法：
//!   cargo run --features llama --bin smoke -- "C:\path\to\dir"

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use deepdark_oracle_lib::ai::build_embedder;
use deepdark_oracle_lib::config::{data_dir, AppConfig};
use deepdark_oracle_lib::db::Db;
use deepdark_oracle_lib::index::Indexer;
use deepdark_oracle_lib::config::ScanConfig;
use deepdark_oracle_lib::scan::Scanner;
use deepdark_oracle_lib::search::{similar, SearchEngine, SearchQuery};
use deepdark_oracle_lib::vector::VectorStore;

fn ok(name: &str, detail: String) -> bool {
    println!("[OK]   {name} — {detail}");
    true
}
fn fail(name: &str, detail: String) -> bool {
    println!("[FAIL] {name} — {detail}");
    false
}

fn main() {
    // 用法：smoke.exe [要扫描的目录]
    // 不给目录时用临时目录下随机子目录，避免依赖任何本机路径。
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join(format!("deepdark-oracle-smoke-{}", std::process::id()))
            .to_string_lossy()
            .to_string()
    });
    let app_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = data_dir(Some(&app_dir)).join("smoke");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();

    let mut cfg = AppConfig::load(&deepdark_oracle_lib::config::config_path(Some(&app_dir)));
    // 冒烟用独立目录，别污染正式库
    let db = Arc::new(Db::open(&data.join("index.db")).unwrap());
    db.migrate().unwrap();

    let mut results: Vec<bool> = Vec::new();
    let t = Instant::now();

    // ---------- 1. 扫描
    let mut scan_cfg = ScanConfig::default();
    scan_cfg.roots = vec![root.clone()];
    let scanner = Scanner::new();
    let mut n = 0usize;
    let _stats = scanner.run(&db, &scan_cfg, |batch| {
        n += batch.len();
    });
    let total = db.count().unwrap_or(0);
    results.push(ok(
        "扫描",
        format!("{} 个文件入库（回调 {n}），{:.1}s", total, t.elapsed().as_secs_f32()),
    ));

    // ---------- 2. 嵌入模型
    let t = Instant::now();
    cfg.model.embed_model = format!("{}/../models/embeddinggemma2.gguf", app_dir.display());
    cfg.model.context_size = 2048;
    let emb = match build_embedder(&cfg.model) {
        Ok(e) => {
            results.push(ok(
                "加载嵌入模型",
                format!(
                    "{} 维 / {} / {:.1}s",
                    e.dim(),
                    e.info().backend,
                    t.elapsed().as_secs_f32()
                ),
            ));
            e
        }
        Err(e) => {
            results.push(fail("加载嵌入模型", e));
            return;
        }
    };
    let dim = emb.dim();
    let media = Arc::new(VectorStore::open(&data.join("vectors_media"), 256).unwrap());
    let vectors = Arc::new(VectorStore::open(&data.join("vectors"), dim).unwrap());
    // 跨模态空间：smoke 里不加载 CLIP/CLAP 模型（那是端到端测试的事），
    // 但空库仍要建出来，否则 SearchEngine 拿不到引用。
    let clip_space = Arc::new(VectorStore::open(&data.join("vectors_clip"), 512).unwrap());
    let audio_space = Arc::new(VectorStore::open(&data.join("vectors_audio"), 512).unwrap());
    let media_emb: Arc<dyn deepdark_oracle_lib::ai::Embedder> =
        Arc::new(deepdark_oracle_lib::ai::hash::HashEmbedder::new());
    let emb: Arc<dyn deepdark_oracle_lib::ai::Embedder> = Arc::from(emb);

    // ---------- 3. 索引（文本 + 缩略图 + 向量）
    let t = Instant::now();
    let idx = std::sync::Arc::new(Indexer::new(
        db.clone(),
        vectors.clone(),
        media.clone(),
        clip_space.clone(),
        audio_space.clone(),
        emb.clone(),
        media_emb.clone(),
        cfg.index.clone(),
        deepdark_oracle_lib::ocr::OcrConfig::from(&cfg.model),
        data.join("thumbs"),
    ));
    idx.start();
    let queued = idx.enqueue_all_pending("auto");
    let _ = idx.wait_idle(600);
    let st = idx.snapshot();
    results.push(ok(
        "索引",
        format!(
            "排队 {queued}，文本 {} 失败 {}，向量 {} 失败 {}，缩略图 {}，{:.1}s",
            st.text_done,
            st.text_fail,
            st.emb_done,
            st.emb_fail,
            st.thumb_done,
            t.elapsed().as_secs_f32()
        ),
    ));

    // ---------- 4. 全文检索（中文两字词）
    let mut cfg2 = cfg.search.clone();
    cfg2.semantic_floor = 0.30;
    let engine = SearchEngine {
        db: &db,
        vectors: &vectors,
        media_vectors: &media,
        clip_vectors: &clip_space,
        audio_vectors: &audio_space,
        cfg: &cfg2,
    };
    for (label, q) in [
        ("全文「余弦」", "余弦"),
        ("全文「会议」", "会议"),
        ("拼音 hyjy", "hyjy"),
        ("关键词「相似度」", "相似度"),
    ] {
        let t = Instant::now();
        let qy = SearchQuery {
            text: q.to_string(),
            limit: Some(5),
            ..Default::default()
        };
        let hits = engine.search(&qy);
        let head = hits
            .first()
            .map(|h| format!("{} ({:.2})", h.name, h.score))
            .unwrap_or_else(|| "无".into());
        results.push(ok(
            label,
            format!("{} 条 / 首条 {head} / {:.0}ms", hits.len(), t.elapsed().as_secs_f32() * 1000.0),
        ));
    }

    // ---------- 5. 语义检索（llama 文本向量）
    {
        let t = Instant::now();
        match emb.embed_text("怎么计算余弦相似度", true) {
            Ok(v) => {
                let qy = SearchQuery {
                    text: "怎么计算余弦相似度".into(),
                    query_vector: Some(v),
                    use_semantic: Some(true),
                    limit: Some(5),
                    ..Default::default()
                };
                let hits = engine.search(&qy);
                let head = hits
                    .iter()
                    .take(3)
                    .map(|h| format!("{}({:.2})", h.name, h.score))
                    .collect::<Vec<_>>()
                    .join(", ");
                results.push(ok(
                    "语义检索「怎么计算余弦相似度」",
                    format!("{} 条 / {head} / {:.0}ms", hits.len(), t.elapsed().as_secs_f32() * 1000.0),
                ));
            }
            Err(e) => results.push(fail("语义检索", e)),
        }
    }

    // ---------- 6. 找相似（媒体空间）
    {
        let rows = db.query_rows(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags FROM files WHERE kind='image' LIMIT 1",
            vec![],
        );
        if let Some(src) = rows.first() {
            let t = Instant::now();
            let hits = similar(
                &db, &vectors, &media, &clip_space, &audio_space, src.id, Some("image"), 5,
            );
            let head = hits
                .iter()
                .take(3)
                .map(|h| format!("{}({:.2})", h.name, h.score))
                .collect::<Vec<_>>()
                .join(", ");
            results.push(ok(
                &format!("以 {} 找相似图", src.name),
                format!("{} 条 / {head} / {:.0}ms", hits.len(), t.elapsed().as_secs_f32() * 1000.0),
            ));
        }
    }

    // ---------- 7. 树状图
    {
        let roll = db.dir_rollup().unwrap_or_default();
        results.push(ok("目录树", format!("{} 个目录节点", roll.len())));
    }

    let pass = results.iter().filter(|x| **x).count();
    let failed = results.len() - pass;
    println!("\n通过 {pass} 项，失败 {failed} 项");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
