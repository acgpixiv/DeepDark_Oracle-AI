//! 命令行接口：`DeepDark_Oracle.exe --cli <子命令>`
//!
//! 打包版是 windowed 程序，没有控制台。为了让它能当 CLI 用，这里在写输出之后
//! 用 `AttachConsole(ATTACH_PARENT_PROCESS)` 挂到调用者的控制台上——这样用户在
//! cmd/PowerShell 里跑就能直接看到结果，双击运行则什么都不发生。
//!
//! 子命令：
//!   scan <目录>...            扫描并入库
//!   search <词> [--kind K]    混合检索（文件名/路径/拼音/全文/语义）
//!   sem <一句话>              纯语义检索
//!   similar <文件路径>        找相似文件（同类型）
//!   stats                     库统计
//!   index [--what auto|text|embed] [--wait]
//!   rules                     列分类规则
//!   gpu [--json]              显卡/后端能力诊断
//!   semgraph [--nodes N]      语义星图聚类诊断（"排序模式"的数据自检）
//!   serve [--port N]          启动局域网服务

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{config_path, data_dir, AppConfig};
use crate::db::Db;
use crate::scan::Scanner;
use crate::search::{SearchEngine, SearchQuery};
use crate::vector::VectorStore;

/// 把 stdout 接到父进程的控制台上（windowed 程序默认没有控制台）。
///
/// 关键：**先检查当前 stdout 是否已经被调用者重定向**（PowerShell `|` / `>`、
/// 批处理的管道重定向都会给进程一个有效的 stdout 句柄）。已经有句柄时绝不能
/// 再 `AttachConsole` + `SetStdHandle`，否则输出会从管道「跳」到控制台缓冲区，
/// 调用方一个字也收不到。
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(dwProcessId: u32) -> i32;
            fn GetStdHandle(nStdHandle: u32) -> *mut core::ffi::c_void;
            fn SetStdHandle(nStdHandle: u32, h: *mut core::ffi::c_void) -> i32;
            fn CreateFileW(
                name: *const u16,
                access: u32,
                share: u32,
                sec: *mut core::ffi::c_void,
                disp: u32,
                flags: u32,
                tmpl: *mut core::ffi::c_void,
            ) -> *mut core::ffi::c_void;
        }
        const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
        const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
        const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;

        let existing = GetStdHandle(STD_OUTPUT_HANDLE);
        // INVALID_HANDLE_VALUE(-1) 与 NULL(0) 都表示「没有可用的 stdout」
        if !existing.is_null() && existing as isize != -1 {
            return; // 已被重定向到管道/文件，保持不动
        }

        // 失败（例如双击运行）就静默继续，不能让它变成 panic
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let h = CreateFileW(
                name.as_ptr(),
                0x4000_0000 | 0x8000_0000, // GENERIC_WRITE | GENERIC_READ
                3,                         // FILE_SHARE_READ | FILE_SHARE_WRITE
                std::ptr::null_mut(),
                3, // OPEN_EXISTING
                0,
                std::ptr::null_mut(),
            );
            if h as isize != -1 {
                SetStdHandle(STD_OUTPUT_HANDLE, h);
                SetStdHandle(STD_ERROR_HANDLE, h);
            }
        }
    }
}

struct Ctx {
    app_dir: PathBuf,
    data_dir: PathBuf,
    cfg: AppConfig,
    db: Arc<Db>,
    vectors: Arc<VectorStore>,
    media: Arc<VectorStore>,
    clip: Arc<VectorStore>,
    audio: Arc<VectorStore>,
    clip_model: Option<Arc<crate::ai::clip::ClipModel>>,
    clap_model: Option<Arc<crate::ai::clap::ClapModel>>,
    /// 惰性加载的文本 embedder：只有 search --semantic / sem / index 会用到。
    /// 用 Mutex 而不是 Option<T> 直接放字段，是因为 Ctx 在 &self 上传给各子命令。
    embedder: parking_lot::Mutex<Option<Arc<dyn crate::ai::Embedder>>>,
}

impl Ctx {
    fn open() -> Result<Self, String> {
        let app_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|x| x.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        let data_dir = data_dir(Some(&app_dir));
        let mut cfg = AppConfig::load(&config_path(Some(&app_dir)));
        // 与主程序一致：没配置模型时在 models\ 下自动探测。
        if cfg.model.embed_model.trim().is_empty() {
            if let Some(found) =
                crate::models::autodetect_embed_model(&cfg.model, &app_dir, &data_dir)
            {
                cfg.model.embed_model = found;
            }
        }
        let db = Db::open(&data_dir.join("index.db")).map_err(|e| format!("打开数据库失败：{e}"))?;
        db.sync_index_roots(&cfg.scan.roots, &cfg.scan.disabled_roots)
            .map_err(|e| format!("同步索引目录失败：{e}"))?;
        // 维度必须和真正要用的 embedder 一致，否则向量库会按错误步长解析。
        //
        // 这里**不能**为了拿维度就去 build_embedder_with_index：那会把 558 MB 的
        // GGUF 完整加载一遍（实测 CLI 每次启动多花 3.4 秒、吃 500+ MB 内存），
        // 而 stats/gpu/search 这些子命令根本用不到模型。改成按证据推断：
        //   1. 已存在的向量库 meta.json（最权威，就是上次真正落盘用的维度）；
        //   2. config 里缓存的 embedding_dim（model_load_inner 成功后写回）；
        //   3. 512 兜底。
        // 只有真正要跑 index/sem 时才在 cmd_index / query_vec 里加载模型。
        let dim = crate::vector::saved_dim_of(&data_dir.join("vectors"))
            .or_else(|| {
                if cfg.model.embedding_dim > 0 {
                    Some(cfg.model.embedding_dim)
                } else {
                    None
                }
            })
            .unwrap_or(512);
        let vectors = VectorStore::open(&data_dir.join("vectors"), dim)
            .map_err(|e| format!("打开向量库失败：{e}"))?;
        let media = VectorStore::open(&data_dir.join("vectors_media"), 256)
            .map_err(|e| format!("打开媒体向量库失败：{e}"))?;
        let clip = VectorStore::open(&data_dir.join("vectors_clip"), 512)
            .map_err(|e| format!("打开跨模态图片向量库失败：{e}"))?;
        let audio = VectorStore::open(&data_dir.join("vectors_audio"), 512)
            .map_err(|e| format!("打开跨模态音频向量库失败：{e}"))?;
        // 命令行跑跨模态：模型缺失就关掉，不阻塞其它子命令。
        //
        // 但**不能静默** —— CLIP/CLAP 依赖一个 ≥1.20 的 onnxruntime.dll，
        // 以前这里全是 `.ok()?`，dll 版本不对就无声无息地退化成"没有
        // 跨模态能力"，用户只会觉得"图片怎么搜不到"。
        let threads = if cfg.model.n_threads > 0 { cfg.model.n_threads } else { 0 };
        let clip_model = match crate::ai::clip::resolve_dir(&cfg.model, &app_dir, &data_dir) {
            Some(d) => match crate::ai::clip::ClipModel::load(&d, threads) {
                Some(m) => Some(Arc::new(m)),
                None => {
                    eprintln!("[cli] CLIP 未能加载（{}），跨模态图片检索不可用", d.display());
                    None
                }
            },
            None => None,
        };
        let clap_model = match crate::ai::clap::resolve_dir(&cfg.model, &app_dir, &data_dir) {
            Some(d) => match crate::ai::clap::ClapModel::load(&d, threads) {
                Some(m) => Some(Arc::new(m)),
                None => {
                    eprintln!("[cli] CLAP 未能加载（{}），声音检索不可用", d.display());
                    None
                }
            },
            None => None,
        };
        Ok(Self {
            app_dir,
            data_dir,
            cfg,
            db: Arc::new(db),
            vectors: Arc::new(vectors),
            media: Arc::new(media),
            clip: Arc::new(clip),
            audio: Arc::new(audio),
            clip_model,
            clap_model,
            embedder: parking_lot::Mutex::new(None),
        })
    }

    fn engine(&self) -> SearchEngine<'_> {
        SearchEngine {
            db: &self.db,
            vectors: &self.vectors,
            media_vectors: &self.media,
            clip_vectors: &self.clip,
            audio_vectors: &self.audio,
            cfg: &self.cfg.search,
        }
    }

    fn apply_root_scope(&self, q: &mut SearchQuery) {
        crate::search::apply_enabled_root_scope(
            q,
            &self.cfg.scan.roots,
            &self.cfg.scan.disabled_roots,
        );
    }

    /// 惰性获取文本 embedder，进程内只加载一次。
    ///
    /// 原来 `query_vec` 每次都 `build_embedder_with_index`（GGUF 558 MB 完整
    /// 加载一遍），而 `Ctx::open` 为了取维度还会再加载一次 —— 一次 `--cli sem`
    /// 要白跑两遍模型。现在统一走这里，最多加载一次。
    fn text_embedder(&self) -> Option<Arc<dyn crate::ai::Embedder>> {
        let mut slot = self.embedder.lock();
        if slot.is_none() {
            match crate::ai::build_embedder_with_index(
                &self.cfg.model,
                &self.cfg.index,
                self.cfg.index.batch_size,
            ) {
                Ok(emb) => *slot = Some(Arc::from(emb)),
                Err(error) => {
                    // 不要静默：模型加载失败是「语义检索没结果」最直接的原因。
                    eprintln!("[cli] 嵌入模型加载失败：{error}");
                    return None;
                }
            }
        }
        slot.clone()
    }

    fn query_vec(&self, text: &str) -> Option<Vec<f32>> {
        self.text_embedder()?.embed_text(text, true).ok()
    }

}

/// 打印一条命中：`分数  类型  大小  名称`
fn print_hits(hits: &[crate::search::Hit], show_snippet: bool) {
    if hits.is_empty() {
        println!("（没有结果）");
        return;
    }
    for h in hits {
        println!(
            "{:>5.2}  {:<4}  {:>9}  {}",
            h.score, h.kind_label, h.size_label, h.name
        );
        println!("        {}", h.path);
        if show_snippet {
            if let Some(s) = h.snippet.as_deref() {
                let s = s.replace('\n', " ");
                let s: String = s.chars().take(120).collect();
                println!("        {s}");
            }
        }
    }
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == name)?;
    args.get(pos + 1).cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// 取所有「不是 flag、也不是 flag 取值」的位置参数。
fn positional(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            // 除开关型 flag 外，其余都吃掉下一个参数
            if !matches!(a.as_str(), "--no-semantic" | "--wait" | "--help" | "-h" | "--json") {
                skip = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

pub fn run(argv: &[String]) -> i32 {
    attach_console();
    // argv[0] 是 exe；--cli 之后的才是子命令
    let rest: Vec<String> = match argv.iter().position(|a| a == "--cli") {
        Some(i) => argv[i + 1..].to_vec(),
        None => Vec::new(),
    };
    if rest.is_empty() {
        print_usage();
        return 2;
    }
    let ctx = match Ctx::open() {
        Ok(c) => c,
        Err(e) => {
            println!("错误：{e}");
            return 1;
        }
    };
    let sub = rest[0].as_str();
    let args = &rest[1..];
    match sub {
        "scan" => cmd_scan(&ctx, args),
        "search" => cmd_search(&ctx, args),
        "sem" => cmd_sem(&ctx, args),
        "similar" => cmd_similar(&ctx, args),
        "stats" => cmd_stats(&ctx),
        "index" => cmd_index(&ctx, args),
        "rules" => cmd_rules(&ctx),
        "gpu" => cmd_gpu(&ctx, args),
        "semgraph" => cmd_semgraph(&ctx, args),
        "serve" => cmd_serve(&ctx, args),
        "-h" | "--help" | "help" => {
            print_usage();
            0
        }
        other => {
            println!("未知子命令：{other}");
            print_usage();
            2
        }
    }
}

fn print_usage() {
    println!("DeepDark_Oracle {} 命令行", crate::APP_DISPLAY_VERSION);
    println!();
    println!("用法：DeepDark_Oracle.exe --cli <子命令> [参数]");
    println!();
    println!("  scan <目录>...              扫描并入库（可多个目录）");
    println!("  search <关键词>             混合检索：文件名/路径/拼音/全文/语义");
    println!("         --kind image|video|audio|doc|text|archive");
    println!("         --limit N           默认 20");
    println!("         --no-semantic       关闭语义（更快）");
    println!("  sem <一句话描述>             纯语义检索");
    println!("  similar <文件路径>           找同类型相似文件");
    println!("  stats                       库统计");
    println!("  index [--what auto|text|embed] [--wait]");
    println!("  rules                       列分类规则");
    println!("  gpu [--json]                显卡/后端能力诊断（写 gpu_report.log）");
    println!("  semgraph [--nodes N] [--json]  语义星图聚类诊断（排序模式的数据自检）");
    println!("  serve [--port N]            启动局域网服务（前台运行）");
}

fn cmd_gpu(_ctx: &Ctx, args: &[String]) -> i32 {
    let as_json = has_flag(args, "--json");
    let report = crate::ai::gpu::probe();
    if as_json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                println!("序列化失败：{e}");
                return 1;
            }
        }
        return 0;
    }
    println!("平台      {} / {}", report.platform, report.arch);
    println!("后端      {}（状态 {}）", report.backend, report.status);
    if report.gpu_name.is_empty() {
        println!("显卡      未检测到");
    } else {
        println!(
            "显卡      {}（{}，{} MiB / 空闲 {} MiB，Tier {}）",
            report.gpu_name, report.vendor, report.vram_mb, report.vram_free_mb, report.tier
        );
    }
    match (&report.cuda_version, &report.cuda_hint) {
        (Some(v), _) => println!("CUDA      {v}"),
        (None, Some(hint)) => println!("CUDA      未安装（建议 {hint}）"),
        (None, None) => println!("CUDA      未安装（本版无需）"),
    }
    // REQ-102：ONNX Runtime 是可选的跨模态扩展。没装是常态，装了就顺带把
    // 该按什么顺序挑 ExecutionProvider 打出来，免得用户以为装了却没生效。
    if report.ort_present {
        println!("ONNX EP   {}", report.ort_providers.join(" → "));
    }
    println!("驱动      {}", report.driver_version.as_deref().unwrap_or("未知"));
    println!();
    println!("结论      {}", report.reason);
    if !report.devices.is_empty() {
        println!();
        println!("设备：");
        for d in &report.devices {
            println!(
                "  [{}] {} · {} · {} MiB{} · Tier {}",
                d.backend,
                d.name,
                d.vendor,
                d.total_mb,
                if d.integrated { " · 核显" } else { "" },
                d.tier
            );
        }
    }
    if !report.backends.is_empty() {
        println!();
        println!("已编入的后端：");
        for b in &report.backends {
            println!(
                "  {:<10} 设备 {}  {} ",
                b.name,
                b.device_count,
                if b.accelerator { "加速器" } else { "纯 CPU" }
            );
        }
    }
    if !report.notes.is_empty() {
        println!();
        println!("提示：");
        for n in &report.notes {
            println!("  - {n}");
        }
    }
    0
}

fn cmd_scan(ctx: &Ctx, args: &[String]) -> i32 {
    let roots: Vec<String> = positional(args);
    if roots.is_empty() {
        println!("用法：--cli scan <目录>...");
        return 2;
    }
    let t = Instant::now();
    println!("扫描 {} 个目录 …", roots.len());
    let mut cfg = ctx.cfg.scan.clone();
    cfg.roots = roots.clone();
    let scanner = Scanner::new();
    let stats = scanner.run(&ctx.db, &cfg, |batch| {
        print!("\r  已入库 +{} 个   ", batch.len());
    });
    println!();
    println!(
        "完成：目录 {}，文件 {}，新增 {}，更新 {}，跳过 {}，错误 {}，已消失 {}，{:.1} MB，{:.1}s",
        stats.dirs,
        stats.files,
        stats.added,
        stats.updated,
        stats.skipped,
        stats.errors,
        stats.missing,
        stats.bytes as f64 / 1_048_576.0,
        t.elapsed().as_secs_f64()
    );
    // 记住这次扫的目录，界面下次打开能直接看到
    let mut cfg_out = ctx.cfg.clone();
    cfg_out.scan.roots = roots;
    let _ = cfg_out.save(&config_path(Some(&ctx.app_dir)));
    println!("下一步：--cli index --wait 建立全文与向量索引");
    0
}

fn cmd_search(ctx: &Ctx, args: &[String]) -> i32 {
    let text: String = positional(args).join(" ");
    if text.is_empty() {
        println!("用法：--cli search <关键词>");
        return 2;
    }
    let limit: usize = parse_flag(args, "--limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let kinds: Vec<String> = match parse_flag(args, "--kind") {
        Some(k) => vec![k],
        None => Vec::new(),
    };
    // Keep CLI search model-free by default, matching the GUI. Embedding is
    // an explicit opt-in so a normal keyword lookup never wakes a model.
    let semantic_requested = has_flag(args, "--semantic") && !has_flag(args, "--no-semantic");
    let mut q = SearchQuery {
        text: text.clone(),
        kinds,
        limit: Some(limit),
        use_semantic: Some(semantic_requested),
        ..Default::default()
    };
    if semantic_requested {
        q.query_vector = ctx.query_vec(&text);
    }
    // 跨模态：命令行默认开启，模型没装时字段为 None，检索段自动跳过。
    if semantic_requested && !has_flag(args, "--no-cross") {
        if let Some(m) = &ctx.clip_model {
            q.clip_query = m.text_vec(&text).ok();
        }
        if let Some(m) = &ctx.clap_model {
            // CLAP 是英文模型：中文查询先映射成英文提示再编码。
            let p = crate::ai::clap::prompt_for(&text);
            q.audio_query = m.text_vec(&p).ok();
        }
    }
    ctx.apply_root_scope(&mut q);
    let t = Instant::now();
    let hits = ctx.engine().search(&q);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("\n搜索「{text}」：{} 条，{ms:.0} ms", hits.len());
    println!("{}", "-".repeat(78));
    print_hits(&hits, true);
    0
}

fn cmd_sem(ctx: &Ctx, args: &[String]) -> i32 {
    let text = positional(args).join(" ");
    if text.trim().is_empty() {
        println!("用法：--cli sem <一句话描述>");
        return 2;
    }
    let Some(vec) = ctx.query_vec(&text) else {
        println!("错误：模型未加载，无法做语义检索（检查 data/config.json 的 model.embed_model）");
        return 1;
    };
    let t = Instant::now();
    let mut q = SearchQuery {
        text,
        query_vector: Some(vec),
        use_semantic: Some(true),
        limit: Some(20),
        ..Default::default()
    };
    ctx.apply_root_scope(&mut q);
    let hits = ctx.engine().search(&q);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("语义检索：{} 条，{ms:.0} ms", hits.len());
    println!("{}", "-".repeat(78));
    print_hits(&hits, false);
    0
}

fn cmd_similar(ctx: &Ctx, args: &[String]) -> i32 {
    let Some(path) = positional(args).into_iter().next() else {
        println!("用法：--cli similar <文件路径>");
        return 2;
    };
    let Some(row) = ctx.db.get_by_path(&path).ok().flatten() else {
        println!("错误：这个文件不在索引里：{path}");
        println!("提示：先跑 --cli scan <所在目录>");
        return 1;
    };
    let t = Instant::now();
    let enabled_roots: Vec<String> = ctx.cfg.scan.roots.iter()
        .filter(|root| !ctx.cfg.scan.disabled_roots.iter().any(|disabled| {
            crate::util::norm_path(disabled).eq_ignore_ascii_case(&crate::util::norm_path(root))
        }))
        .cloned()
        .collect();
    let hits = crate::search::similar_scoped(
        &ctx.db,
        &ctx.vectors,
        &ctx.media,
        &ctx.clip,
        &ctx.audio,
        row.id,
        None,
        10,
        if ctx.cfg.scan.roots.is_empty() { None } else { Some(&enabled_roots) },
    );
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("与「{}」相似：{} 条，{ms:.0} ms", row.name, hits.len());
    println!("{}", "-".repeat(78));
    print_hits(&hits, false);
    0
}

fn cmd_stats(ctx: &Ctx) -> i32 {
    let total = ctx.db.count().unwrap_or(0);
    let bytes = ctx.db.total_bytes().unwrap_or(0);
    println!("DeepDark_Oracle {}", crate::APP_DISPLAY_VERSION);
    println!("数据目录：{}", ctx.data_dir.display());
    println!("文件：{total} 个，{:.2} MB", bytes as f64 / 1_048_576.0);
    println!("正文已抽取：{}", ctx.db.count_state("text", "ok").unwrap_or(0));
    println!("向量已完成：{}", ctx.db.count_state("embed", "ok").unwrap_or(0));
    println!(
        "文本向量库：{} 条 / {} 维",
        ctx.vectors.len(),
        ctx.vectors.dim()
    );
    println!(
        "媒体向量库：{} 条 / {} 维",
        ctx.media.len(),
        ctx.media.dim()
    );
    if let Ok(kinds) = ctx.db.kind_stats() {
        println!("按类型：");
        for (k, n, b) in kinds {
            println!(
                "  {:<8} {:>6} 个  {:>10.2} MB",
                crate::util::kind_label(&k),
                n,
                b as f64 / 1_048_576.0
            );
        }
    }
    0
}

fn cmd_index(ctx: &Ctx, args: &[String]) -> i32 {
    let what = parse_flag(args, "--what").unwrap_or_else(|| "auto".into());
    let Some(emb) = ctx.text_embedder() else {
        println!("错误：模型未加载（检查 data/config.json 的 model.embed_model）");
        return 1;
    };
    // 建索引前把真实后端与设备打出来：用户最常问的就是「GPU 到底有没有在被用」。
    // 必须显式 flush：stdout 接管道时是块缓冲，进程被 Ctrl-C / timeout 掐掉
    // 时缓冲区整段丢失，用户看到的现象就是「什么都没打印就退出了」。
    let info = emb.info();
    println!(
        "模型：{} · {} 维 · {} · {} · 模态 {}",
        info.model, info.dim, info.backend, info.device,
        if info.modalities.is_empty() { "text".to_string() } else { info.modalities.join("/") }
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let idx = crate::index::Indexer::new(
        ctx.db.clone(),
        ctx.vectors.clone(),
        ctx.media.clone(),
        ctx.clip.clone(),
        ctx.audio.clone(),
        emb,
        Arc::new(crate::ai::hash::HashEmbedder::new()),
        ctx.cfg.index.clone(),
        ctx.data_dir.join("thumbs"),
    );
    // CLI 也必须走 GUI 那套批尺寸推导，否则永远用配置文件里的 8。
    //
    // 实测：GUI 在 7 GiB 空闲显存上会调到 32，而 CLI 一直是 8 ——
    // 同一台机器、同一份数据，命令行索引的吞吐只有界面的三分之一。
    idx.set_batch_size(crate::ai::effective_batch_size(
        &ctx.cfg.model,
        ctx.cfg.index.batch_size,
    ));
    idx.set_worker_threads(crate::ai::effective_parallel_threads(&ctx.cfg.model));
    idx.set_cross_modal(ctx.clip_model.clone(), ctx.clap_model.clone());
    let idx = Arc::new(idx);
    let mut queued = idx.enqueue_all_pending(&what);
    // 维度变化会把向量库清空，但 embed_state 仍是 ok → 查不到待办。
    // 这里补一道自愈：库空了但有「已完成」的记录，就全部重排。
    if queued == 0 && has_flag(args, "--force") {
        let n = ctx.db.reset_states(&what).unwrap_or(0);
        queued = idx.enqueue_all_pending(&what);
        println!("强制重建：重置 {n} 条记录");
    } else if queued == 0 && what != "text" && ctx.vectors.len() == 0 {
        let n = ctx.db.reset_states("embed").unwrap_or(0);
        if n > 0 {
            queued = idx.enqueue_all_pending("embed");
            println!("检测到文本向量库为空，已重置 {n} 条记录重新入队");
        }
    }
    println!("已入队 {queued} 个文件（{what}）");
    if queued == 0 {
        return 0;
    }
    // 之前只有带 --wait 才等，默认路径在 start() 之后立刻 return，
    // 进程退出把 worker 线程一起杀掉 —— 用户看到的现象就是「索引跑了
    // 一下就没动静、GPU 占用几乎为零」。所以默认改成等索引跑完并打
    // 进度；只想排队不等的话显式加 --no-wait。
    if has_flag(args, "--no-wait") {
        idx.start();
        println!("已在后台启动；加 --wait 或去掉 --no-wait 可等待完成。");
        return 0;
    }
    idx.start();
    // 0 表示不限时：1 TB 级别的库本来就要跑很久，硬性超时只会把
    // 一次本来能完成的索引切成半截。需要上限时用 --timeout <秒>。
    let timeout_secs = parse_flag(args, "--timeout")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let ok = wait_with_progress(&idx, timeout_secs);
    let s = idx.snapshot();
    println!();
    println!(
        "{}：正文 {}（失败 {}），向量 {}（失败 {}），跨模态 {}（失败 {}），缩略图 {}，用时 {}s",
        if ok { "完成" } else { "超时" },
        s.text_done,
        s.text_fail,
        s.emb_done,
        s.emb_fail,
        s.cross_done,
        s.cross_fail,
        s.thumb_done,
        s.elapsed_seconds
    );
    if let Some(error) = s.last_error.as_deref() {
        println!("最后一条错误：{error}");
    }
    idx.stop();
    if ok {
        0
    } else {
        1
    }
}

/// 等索引跑完并每秒刷新一行进度。timeout_secs 为 0 表示不限时。
fn wait_with_progress(idx: &crate::index::Indexer, timeout_secs: u64) -> bool {
    let started = std::time::Instant::now();
    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(started + std::time::Duration::from_secs(timeout_secs))
    };
    let mut last_report = started;
    loop {
        let snapshot = idx.snapshot();
        if snapshot.queued == 0 && !snapshot.running {
            // 与 wait_idle 同样的宽限：队列刚空时 worker 可能还没置 busy。
            std::thread::sleep(std::time::Duration::from_millis(200));
            let settled = idx.snapshot();
            if settled.queued == 0 && !settled.running {
                return true;
            }
        }
        if last_report.elapsed() >= std::time::Duration::from_secs(1) {
            last_report = std::time::Instant::now();
            // 先清行再打印，避免上一次更长的行留下尾巴。
            print!(
                "\r  索引中 · 剩余 {:<7} · 正文 {:<7} · 向量 {:<7} · 失败 {:<5} · 用时 {}s\x1b[K",
                snapshot.queued,
                snapshot.text_done,
                snapshot.emb_done,
                snapshot.emb_fail,
                snapshot.elapsed_seconds
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        if let Some(deadline) = deadline {
            if std::time::Instant::now() > deadline {
                return false;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

fn cmd_rules(ctx: &Ctx) -> i32 {
    match ctx.db.list_rules() {
        Ok(rules) if rules.is_empty() => println!("（还没有分类规则，可在界面的「设置 → 分类规则」里添加）"),
        Ok(rules) => {
            println!("{:<20} {:<10} {:<12} {:<12}", "名称", "字段", "操作", "归类到");
            for r in rules {
                println!(
                    "{:<20} {:<10} {:<12} {:<12}",
                    r.name, r.field, r.op, r.category
                );
            }
        }
        Err(e) => {
            println!("读取规则失败：{e}");
            return 1;
        }
    }
    0
}

fn cmd_serve(ctx: &Ctx, args: &[String]) -> i32 {
    let mut cfg = ctx.cfg.server.clone();
    if let Some(p) = parse_flag(args, "--port").and_then(|s| s.parse().ok()) {
        cfg.port = p;
    }
    println!("正在启动局域网服务 …");
    let state = crate::server::ServerState {
        db: ctx.db.clone(),
        vectors: ctx.vectors.clone(),
        media_vectors: ctx.media.clone(),
        clip_vectors: ctx.clip.clone(),
        audio_vectors: ctx.audio.clone(),
        cfg: Arc::new(ctx.cfg.search.clone()),
        scan_roots: ctx.cfg.scan.roots.clone(),
        disabled_roots: ctx.cfg.scan.disabled_roots.clone(),
        token: cfg.token.clone(),
        allow_write: cfg.allow_write,
        data_dir: ctx.data_dir.clone(),
    };
    let server = crate::server::LanServer::new();
    match server.start(&cfg, state) {
        Ok(url) => {
            println!("已启动：{url}");
            println!("按 Ctrl+C 停止");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        Err(e) => {
            println!("启动失败：{e}");
            1
        }
    }
}

/// `semgraph`：把语义星图的构建过程跑一遍并打印统计。
///
/// 为什么要有这个子命令：语义星图的结果是"一团点"，在 GUI 里只能靠肉眼
/// 判断"看起来像不像一坨"。这里直接把可量化的指标打出来 ——
/// 簇数、每簇大小、连通分量数、孤立点数、平均度数 ——
/// 一坨和好布局的区别在这些数字上是可判定的：
/// 好布局连通分量 = 1、孤立点 = 0、每簇规模分布均匀。
fn cmd_semgraph(ctx: &Ctx, args: &[String]) -> i32 {
    let mut max_nodes = 260usize;
    let mut as_json = false;
    let mut threshold = 0.55f64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--nodes" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    max_nodes = v;
                }
                i += 2;
            }
            "--threshold" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f64>().ok()) {
                    threshold = v;
                }
                i += 2;
            }
            "--json" => {
                as_json = true;
                i += 1;
            }
            _ => i += 1,
        }
    }

    // CLI 里没有 AppState（那需要完整的 Tauri 上下文），但语义星图真正需要的
    // 只有"向量库 + 数据库"两样，Ctx 正好都有 —— 所以直接复用同一套算法。

    // 与 star_semantic 一致：按每个向量空间各自取样。
    // （只看 files.embed_state 会拿到一堆"状态为 ok 但向量在别的空间"的记录。）
    let spaces_src: Vec<(&str, &Arc<VectorStore>)> = vec![
        ("text", &ctx.vectors),
        ("media", &ctx.media),
        ("clip", &ctx.clip),
        ("audio", &ctx.audio),
    ];
    let spaces_src: Vec<(&str, &Arc<VectorStore>)> = spaces_src.into_iter().filter(|(_, s)| s.len() > 0).collect();
    if spaces_src.is_empty() {
        println!("四个向量空间都是空的，先建索引再试。");
        return 1;
    }
    let total_len: usize = spaces_src.iter().map(|(_, s)| s.len()).sum();
    let mut picked: Vec<(i64, &str)> = Vec::new();
    let mut got: Vec<(i64, Vec<f32>)> = Vec::new();
    for (name, store) in &spaces_src {
        let quota = if spaces_src.len() == 1 {
            max_nodes
        } else {
            (((store.len() as f64 / total_len as f64) * max_nodes as f64).round() as usize).max(12)
        };
        let ids = store.all_ids();
        let rows = ctx.db.files_by_ids(&ids);
        let pool: Vec<(i64, String)> = rows.into_iter().map(|r| (r.id, r.dir.clone())).collect();
        let take = if pool.len() <= quota {
            pool.iter().map(|(id, _)| *id).collect::<Vec<_>>()
        } else {
            crate::star::sample_ids_for_cli(&pool, quota)
        };
        for id in take {
            picked.push((id, name));
        }
        for (id, v) in store.get_many(&picked.iter().filter(|(_, n)| n == name).map(|(i, _)| *i).collect::<Vec<_>>()) {
            got.push((id, v));
        }
    }
    let picked_ids: Vec<i64> = picked.iter().map(|(id, _)| *id).collect();
    let available = picked_ids.len();
    let rows = ctx.db.files_by_ids(&picked_ids);
    let meta: std::collections::HashMap<i64, crate::db::FileRow> =
        rows.into_iter().map(|r| (r.id, r)).collect();

    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    // 维度可能不同（不同向量空间），聚类前只保留同维度的点。
    let mut dim_count: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (_, v) in got.iter() {
        *dim_count.entry(v.len()).or_insert(0) += 1;
    }
    let keep_dim = dim_count.iter().max_by_key(|(_, c)| **c).map(|(d, _)| *d).unwrap_or(0);
    for (id, mut v) in got {
        if !meta.contains_key(&id) || v.is_empty() {
            continue;
        }
        let _ = id;
        if v.len() != keep_dim {
            continue;
        }
        crate::star::normalize_vec_cli(&mut v);
        vectors.push(v);
        names.push(meta.get(&id).map(|r| r.name.clone()).unwrap_or_default());
    }
    let n = vectors.len();
    if n < 4 {
        println!("可用向量只有 {n} 条，太少，聚类没有意义。");
        return 1;
    }

    let k = ((n as f64 / 2.0).sqrt() as usize).clamp(6, 24).min(n);
    let (assign, _centers) = crate::star::kmeans_for_cli(&vectors, k, 12);

    // ---- 统计 --------------------------------------------------------
    let mut cluster_size: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &c in assign.iter() {
        *cluster_size.entry(c).or_insert(0) += 1;
    }
    let mut sizes: Vec<usize> = cluster_size.values().copied().collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let non_empty = sizes.len();

    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut Vec<usize>, x: usize) -> usize {
        let mut r = x;
        while p[r] != r {
            r = p[r];
        }
        let mut c = x;
        while p[c] != r {
            let nxt = p[c];
            p[c] = r;
            c = nxt;
        }
        r
    }
    fn union(p: &mut Vec<usize>, a: usize, b: usize) {
        let ra = find(p, a);
        let rb = find(p, b);
        if ra != rb {
            p[ra] = rb;
        }
    }

    let mut deg = vec![0usize; n];
    let mut edges = 0usize;
    let mut by_cluster: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for (i, &c) in assign.iter().enumerate() {
        by_cluster.entry(c).or_default().push(i);
    }
    for i in 0..n {
        let ci = assign[i];
        let mut cand: Vec<(usize, f32)> = by_cluster[&ci]
            .iter()
            .filter(|&&j| j != i)
            .map(|&j| (j, crate::star::cosine_cli(&vectors[i], &vectors[j])))
            .collect();
        cand.sort_by(|a, b| b.1.total_cmp(&a.1));
        let k1 = (by_cluster[&ci].len() / 8).clamp(2, 6);
        for (j, sim) in cand.into_iter().take(k1) {
            if j <= i || sim < (threshold * 0.75) as f32 {
                continue;
            }
            union(&mut parent, i, j);
            deg[i] += 1;
            deg[j] += 1;
            edges += 1;
        }
    }
    let mut bridged = 0usize;
    for i in 0..n {
        if deg[i] > 0 {
            continue;
        }
        bridged += 1;
        let mut best: Vec<(usize, f32)> = (0..n)
            .filter(|&j| j != i)
            .map(|j| (j, crate::star::cosine_cli(&vectors[i], &vectors[j])))
            .collect();
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (j, _) in best.into_iter().take(2) {
            union(&mut parent, i, j);
            deg[i] += 1;
            deg[j] += 1;
            edges += 1;
        }
    }
    // 与 build_semantic 一致：按真实连通分量收口（簇内也可能分裂）。
    {
        let mut roots: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            roots.entry(r).or_default().push(i);
        }
        if roots.len() > 1 {
            let mut reps: Vec<usize> = roots.values().map(|m| m[0]).collect();
            reps.sort_unstable();
            for w in reps.windows(2) {
                union(&mut parent, w[0], w[1]);
                deg[w[0]] += 1;
                deg[w[1]] += 1;
                edges += 1;
            }
        }
    }
    let mut comps: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        comps.insert(r);
    }
    let isolated = deg.iter().filter(|&&d| d == 0).count();
    let avg_deg = deg.iter().sum::<usize>() as f64 / n as f64;

    if as_json {
        println!(
            "{{\"available\":{},\"sampled\":{},\"clusters\":{},\"edges\":{},\"components\":{},\"isolated\":{},\"bridged\":{},\"avgDegree\":{:.2},\"sizes\":{:?}}}",
            available, n, non_empty, edges, comps.len(), isolated, bridged, avg_deg, sizes
        );
        return 0;
    }

    println!("语义星图诊断");
    println!("  可用向量     {}", available);
    println!("  本次取样     {}", n);
    println!("  簇数         {}（目标 {}）", non_empty, k);
    println!("  边数         {}", edges);
    println!("  连通分量     {}  <- 必须为 1，否则图上会有漂在角落的孤岛", comps.len());
    println!("  孤立点       {}（已桥接 {}）", isolated, bridged);
    println!("  平均度数     {:.2}", avg_deg);
    println!("  簇规模       {:?}", &sizes[..sizes.len().min(12)]);
    println!();
    println!("  各簇代表（前 5 个）：");
    let mut shown = 0;
    for (ci, idxs) in by_cluster.iter() {
        if shown >= 5 {
            break;
        }
        shown += 1;
        let sample: Vec<&str> = idxs.iter().take(3).map(|&i| names[i].as_str()).collect();
        println!("    簇 {}（{} 个）  {}", ci, idxs.len(), sample.join(" · "));
    }
    0
}