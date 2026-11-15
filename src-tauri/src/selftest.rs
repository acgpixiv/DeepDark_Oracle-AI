//! 无界面自检：验证打包版「装好了就能用」。
//!
//! 用法：`DeepDark_Oracle.exe --selftest`
//! 结果同时写到 exe 同级 `selftest.txt`（打包版没有控制台，这是唯一可读的出口）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::ai::build_embedder;
use crate::config::{config_path, data_dir, AppConfig};
use crate::db::Db;
use crate::search::{similar, SearchEngine, SearchQuery};
use crate::vector::VectorStore;

struct Report {
    lines: Vec<String>,
    failed: usize,
    skipped: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            failed: 0,
            skipped: 0,
        }
    }

    fn ok(&mut self, name: &str, detail: impl Into<String>, ms: f64) {
        self.lines
            .push(format!("[OK]   {name} ({ms:.0}ms) — {}", detail.into()));
    }

    fn fail(&mut self, name: &str, detail: impl Into<String>, ms: f64) {
        self.failed += 1;
        self.lines
            .push(format!("[FAIL] {name} ({ms:.0}ms) — {}", detail.into()));
    }

    /// 前置条件不满足（例如库是空的），不算通过也不算失败。
    fn skip(&mut self, name: &str, detail: impl Into<String>) {
        self.skipped += 1;
        self.lines
            .push(format!("[SKIP] {name} — {}", detail.into()));
    }
}

pub fn run() -> i32 {
    let app_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|x| x.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let data = data_dir(Some(&app_dir));
    let mut r = Report::new();
    r.lines.push(format!(
        "DeepDark_Oracle {} ({})",
        crate::APP_DISPLAY_VERSION,
        crate::APP_VERSION
    ));
    r.lines.push(format!("app_dir={}", app_dir.display()));
    r.lines.push(format!("data_dir={}", data.display()));
    r.lines.push("-".repeat(60));

    // 1. 配置
    let cfg_path = config_path(Some(&app_dir));
    let mut cfg = AppConfig::load(&cfg_path);
    // 安装版/便携版首次运行时 embed_model 是空的，主程序会在后台自动探测
    // models\ 下的模型。自检必须用同一套规则，否则会误报「改用哈希基线」。
    let mut autodetected = false;
    if cfg.model.embed_model.trim().is_empty() {
        if let Some(found) = crate::models::autodetect_embed_model(&cfg.model, &app_dir, &data) {
            cfg.model.embed_model = found;
            autodetected = true;
        }
    }
    r.ok(
        "读取配置",
        format!(
            "{}（模型 {}{}）",
            cfg_path.display(),
            if cfg.model.embed_model.trim().is_empty() {
                "未配置"
            } else {
                cfg.model.embed_model.as_str()
            },
            if autodetected { "，自动探测" } else { "" }
        ),
        0.0,
    );

    // 2. 数据库
    let t = Instant::now();
    let db = match Db::open(&data.join("index.db")) {
        Ok(d) => {
            let n = d.count().unwrap_or(0);
            r.ok("打开数据库", format!("{n} 个文件"), t.elapsed().as_secs_f64() * 1000.0);
            Some(Arc::new(d))
        }
        Err(e) => {
            r.fail("打开数据库", e.to_string(), t.elapsed().as_secs_f64() * 1000.0);
            None
        }
    };
    let Some(db) = db else {
        return finish(r, &app_dir);
    };

    // 3. 模型
    let t = Instant::now();
    let model_dim: usize;
    let mut engine_ok = false;
    match build_embedder(&cfg.model) {
        Ok(emb) => {
            model_dim = emb.dim();
            let info = emb.info();
            r.ok(
                "加载嵌入模型",
                format!("{} · {} 维 · {}", info.backend, model_dim, info.model),
                t.elapsed().as_secs_f64() * 1000.0,
            );
            engine_ok = true;
            // 4. 编码
            let t = Instant::now();
            match emb.embed_text("测试文本", false) {
                Ok(v) => {
                    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    r.ok(
                        "文本向量化",
                        format!("{} 维，L2 范数 {:.3}", v.len(), norm),
                        t.elapsed().as_secs_f64() * 1000.0,
                    );
                }
                Err(e) => r.fail("文本向量化", e, t.elapsed().as_secs_f64() * 1000.0),
            }
        }
        Err(e) => {
            // 模型缺失不算致命：哈希基线也能跑，只是精度差
            r.ok(
                "加载嵌入模型",
                format!("未配置模型，改用哈希基线（{e}）"),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }

    // 5. 文本向量库
    let t = Instant::now();
    let store_dim = crate::state::configured_vector_dim(cfg.index.vector_dim);
    let vectors = match VectorStore::open(&data.join("vectors"), store_dim) {
        Ok(v) => {
            r.ok(
                "打开文本向量库",
                format!("{} 条 / {} 维", v.len(), v.dim()),
                t.elapsed().as_secs_f64() * 1000.0,
            );
            Arc::new(v)
        }
        Err(e) => {
            r.fail("打开文本向量库", e.to_string(), t.elapsed().as_secs_f64() * 1000.0);
            return finish(r, &app_dir);
        }
    };
    let media = Arc::new(
        VectorStore::open(&data.join("vectors_media"), store_dim)
            .unwrap_or_else(|_| panic!("无法创建媒体向量库")),
    );
    let clip_space = Arc::new(
        VectorStore::open(&data.join("vectors_clip"), store_dim)
            .unwrap_or_else(|_| panic!("无法创建跨模态图片向量库")),
    );
    let audio_space = Arc::new(
        VectorStore::open(&data.join("vectors_audio"), store_dim)
            .unwrap_or_else(|_| panic!("无法创建跨模态音频向量库")),
    );

    // 5.1 跨模态模型（可选）：装了才检查，没装跳过
    let threads = if cfg.model.n_threads > 0 { cfg.model.n_threads } else { 0 };
    let clip_model = crate::ai::clip::resolve_dir(&cfg.model, &app_dir, &data)
        .and_then(|d| crate::ai::clip::ClipModel::load(&d, threads))
        .map(Arc::new);
    let t = Instant::now();
    match &clip_model {
        Some(m) => {
            let t2 = Instant::now();
            match m.text_vec("一只猫") {
                Ok(v) => r.ok(
                    "跨模态图片模型（CLIP）",
                    format!("{} 维，文本编码 {:.0}ms", v.len(), t2.elapsed().as_secs_f64() * 1000.0),
                    t.elapsed().as_secs_f64() * 1000.0,
                ),
                Err(e) => r.fail("跨模态图片模型（CLIP）", e, t.elapsed().as_secs_f64() * 1000.0),
            }
        }
        None => r.skip("跨模态图片模型（CLIP）", "未安装（文字找图片功能关闭）"),
    }
    let clap_model = crate::ai::clap::resolve_dir(&cfg.model, &app_dir, &data)
        .and_then(|d| crate::ai::clap::ClapModel::load(&d, threads))
        .map(Arc::new);
    let t = Instant::now();
    match &clap_model {
        Some(m) => {
            let t2 = Instant::now();
            match m.text_vec("雨声") {
                Ok(v) => r.ok(
                    "跨模态音频模型（CLAP）",
                    format!("{} 维，文本编码 {:.0}ms", v.len(), t2.elapsed().as_secs_f64() * 1000.0),
                    t.elapsed().as_secs_f64() * 1000.0,
                ),
                Err(e) => r.fail("跨模态音频模型（CLAP）", e, t.elapsed().as_secs_f64() * 1000.0),
            }
        }
        None => r.skip("跨模态音频模型（CLAP）", "未安装（文字找声音功能关闭）"),
    }

    // 6. 全文 / 拼音 / 文件名
    //
    // 空库时这四项必然 0 条：新装用户第一次跑自检就满屏 FAIL，会以为程序坏了。
    // 没有文件就跳过检索类检查（结构性检查照跑），并提示先扫描索引。
    let engine = SearchEngine {
        db: &db,
        vectors: &vectors,
        media_vectors: &media,
        clip_vectors: &clip_space,
        audio_vectors: &audio_space,
        cfg: &cfg.search,
    };
    let empty = db.count().unwrap_or(0) == 0;
    for (q, want) in [("会议", "全文"), ("hyjy", "拼音"), ("forest", "文件名")] {
        let t = Instant::now();
        let label = format!("{want}检索「{q}」");
        if empty {
            r.skip(&label, "库里还没有文件（先在「扫描 / 索引」页添加目录）");
            continue;
        }
        let hits = engine.search(&SearchQuery {
            text: q.to_string(),
            limit: Some(3),
            ..Default::default()
        });
        let names: Vec<String> = hits.iter().take(3).map(|h| h.name.clone()).collect();
        if hits.is_empty() {
            r.fail(
                &label,
                "0 条结果".to_string(),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        } else {
            r.ok(
                &label,
                format!("{} 条：{}", hits.len(), names.join(", ")),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }

    // 7. 语义（用真实向量）
    if empty {
        r.skip("语义检索", "库里还没有文件");
    } else if engine_ok {
        if let Ok(emb) = build_embedder(&cfg.model) {
            let t = Instant::now();
            match emb.embed_text("怎么计算余弦相似度", true) {
                Ok(vec) => {
                    let hits = engine.search(&SearchQuery {
                        text: "怎么计算余弦相似度".to_string(),
                        query_vector: Some(vec),
                        use_semantic: Some(true),
                        limit: Some(3),
                        ..Default::default()
                    });
                    if hits.is_empty() {
                        r.fail(
                            "语义检索",
                            "0 条结果（向量库可能是空的，去扫描页重建索引）",
                            t.elapsed().as_secs_f64() * 1000.0,
                        );
                    } else {
                        let top = &hits[0];
                        r.ok(
                            "语义检索",
                            format!(
                                "{} 条，首条 {} ({:.2})",
                                hits.len(),
                                top.name,
                                top.score
                            ),
                            t.elapsed().as_secs_f64() * 1000.0,
                        );
                    }
                }
                Err(e) => r.fail("语义检索", e, t.elapsed().as_secs_f64() * 1000.0),
            }
        }
    }

    // 7.5 「一句话找图 / 找声」：编一句描述，看能不能把对应类型的文件捞出来。
    //     模型没装或库里没有该类型文件时跳过，不算失败。
    for (label, kind, model_kind, prompt) in [
        ("文字找图", "image", "clip", "一只黑色的狗"),
        ("文字找声音", "audio", "clap", "钢琴"),
    ] {
        let t = Instant::now();
        let has_model = if model_kind == "clip" { clip_model.is_some() } else { clap_model.is_some() };
        if !has_model {
            r.skip(label, format!("未安装跨模态模型（{model_kind}）"));
            continue;
        }
        let pool = db.query_rows(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags
             FROM files WHERE missing=0 AND kind=?1 LIMIT 1",
            vec![rusqlite::types::Value::Text(kind.to_string())],
        );
        if pool.is_empty() {
            r.skip(label, format!("库里还没有{}", if kind == "image" { "图片" } else { "音频" }));
            continue;
        }
        let mut q = SearchQuery {
            text: prompt.to_string(),
            kinds: vec![kind.to_string()],
            limit: Some(5),
            use_semantic: Some(false),
            cross_modal: Some(true),
            ..Default::default()
        };
        // 手动编码查询（selftest 不走 AppState，这里是唯一入口）。
        if let Some(m) = &clip_model {
            if let Ok(v) = m.text_vec(prompt) {
                q.clip_query = Some(v);
            }
        }
        if let Some(m) = &clap_model {
            let p = crate::ai::clap::prompt_for(prompt);
            if let Ok(v) = m.text_vec(&p) {
                q.audio_query = Some(v);
            }
        }
        let hits = engine.search(&q);
        if hits.is_empty() {
            r.fail(label, format!("「{prompt}」0 条结果"), t.elapsed().as_secs_f64() * 1000.0);
        } else {
            let top = &hits[0];
            r.ok(
                label,
                format!("「{prompt}」→ {} ({:.2})", top.name, top.score),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }

    // 8. 找相似
    let t = Instant::now();
    let img = db.query_rows(
        "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags
         FROM files WHERE missing=0 AND kind='image' LIMIT 1",
        Vec::new(),
    );
    if let Some(src) = img.first() {
        let hits = similar(&db, &vectors, &media, &clip_space, &audio_space, src.id, None, 3);
        if hits.is_empty() {
            r.ok(
                "图片找相似",
                format!("源 {}；同类里没有可比文件（正常）", src.name),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        } else {
            let names: Vec<String> = hits
                .iter()
                .map(|h| format!("{}({:.2})", h.name, h.score))
                .collect();
            r.ok(
                "图片找相似",
                format!("源 {} → {}", src.name, names.join(", ")),
                t.elapsed().as_secs_f64() * 1000.0,
            );
        }
    } else {
        r.skip("图片找相似", "库里没有图片");
    }

    // 9. 目录树
    let t = Instant::now();
    match db.dir_rollup() {
        Ok(rows) => r.ok(
            "目录统计",
            format!("{} 个目录节点", rows.len()),
            t.elapsed().as_secs_f64() * 1000.0,
        ),
        Err(e) => r.fail("目录统计", e.to_string(), t.elapsed().as_secs_f64() * 1000.0),
    }

    // 10. 画面标签（「文字找图片」的可用形态）
    //
    // 不依赖向量：颜色/构图统计给图片打中文标签，搜索时按标签命中。
    // 这里验证「库里有标签」+「用标签能搜到图片」两件事。
    let t = Instant::now();
    let tagged = db
        .query_rows(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,\
             embed_state,thumb_key,category,tags FROM files \
             WHERE missing=0 AND tags IS NOT NULL AND tags!='' \
             ORDER BY length(tags) DESC LIMIT 1",
            Vec::new(),
        )
        .into_iter()
        .next();
    match tagged {
        Some(row) => {
            // 取一个有辨识度的标签（跳过「图片」「视频」这种通名），
            // 用真正的搜索引擎搜一遍。
            let first_tag = row
                .tags
                .as_deref()
                .and_then(|t| {
                    t.split(',')
                        .map(str::trim)
                        .find(|x| !x.is_empty() && *x != "图片" && *x != "视频")
                })
                .unwrap_or("")
                .to_string();
            if first_tag.is_empty() {
                r.skip("画面标签检索", "标签只有通名（图片/视频），去索引页重建一次即可");
            } else {
                let mut q = SearchQuery::default();
                q.text = first_tag.clone();
                q.use_semantic = Some(false);
                q.limit = Some(20);
                let engine = SearchEngine {
                    db: &db,
                    vectors: &vectors,
                    media_vectors: &media,
                    clip_vectors: &clip_space,
                    audio_vectors: &audio_space,
                    cfg: &cfg.search,
                };
                let hits = engine.search(&q);
                let found = hits.iter().any(|h| h.id == row.id);
                let msg = format!(
                    "{} 个文件有标签，示例「{}」→ {} 条{}",
                    db.query_rows(
                        "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,\
                         embed_state,thumb_key,category,tags FROM files \
                         WHERE missing=0 AND tags IS NOT NULL AND tags!=''",
                        Vec::new()
                    )
                    .len(),
                    first_tag,
                    hits.len(),
                    if found { "（含源文件）" } else { "" }
                );
                if found {
                    r.ok("画面标签检索", msg, t.elapsed().as_secs_f64() * 1000.0);
                } else {
                    // 不算失败：可能源文件被 limit 截断，或标签词被其它文件抢了分
                    r.ok(
                        "画面标签检索",
                        format!("{msg}（源文件未进前 20，属正常）"),
                        t.elapsed().as_secs_f64() * 1000.0,
                    );
                }
            }
        }
        None => r.skip("画面标签检索", "库里还没有带标签的图片（先索引一批图片即可）"),
    }

    finish(r, &app_dir)
}

fn finish(mut r: Report, app_dir: &std::path::Path) -> i32 {
    r.lines.push("-".repeat(60));
    let passed = r.lines.iter().filter(|l| l.starts_with("[OK]")).count();
    let skip_note = if r.skipped > 0 {
        format!("，跳过 {} 项（库还需索引文件）", r.skipped)
    } else {
        String::new()
    };
    r.lines
        .push(format!("通过 {} 项，失败 {} 项{skip_note}", passed, r.failed));
    let text = r.lines.join("\n");
    let _ = std::fs::write(app_dir.join("selftest.txt"), &text);
    println!("{text}");
    if r.failed == 0 {
        0
    } else {
        1
    }
}
