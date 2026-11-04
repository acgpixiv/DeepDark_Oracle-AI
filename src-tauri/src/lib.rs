//! DeepDark_Oracle —— Tauri 2 + Vue 3 + llama.cpp 的本地 AI 磁盘搜索工具。
pub mod ai;
pub mod cli;
pub mod cmd;
pub mod config;
pub mod db;
pub mod disk;
pub mod fileops;
pub mod index;
pub mod media;
pub mod models;
pub mod py;
pub mod scan;
pub mod search;
pub mod selftest;
pub mod server;
pub mod star;
pub mod state;
pub mod textract;
pub mod util;
pub mod vector;
pub mod vision;
pub mod watch;
pub mod workspace;

pub const APP_VERSION: &str = "0.0.28";
pub const APP_DISPLAY_VERSION: &str = "0.28";

use std::sync::Arc;
use tauri::Manager;

pub fn run() {
    let app_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let state = match state::AppState::new(app_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("初始化失败: {e}");
            // 兜底：把错误写文件，方便无控制台的发布版排查
            let _ = std::fs::write("DeepDark_Oracle-error.txt", format!("初始化失败: {e}"));
            return;
        }
    };
    let _ = state.db.log("app", &format!("DeepDark_Oracle {} 启动", APP_DISPLAY_VERSION));
    let _ = state.rebuild_indexer();
    // GPU 能力探测（REQ-093）：报告写进数据目录的 gpu_report.log，同时把结论
    // 记到活动日志。必须在后台线程里做 —— 探测会起 nvidia-smi 子进程，
    // 冷启动时这段可能耗到几百毫秒，放在主线程会让窗口白屏更久。
    {
        let dir = state.data_dir.clone();
        // REQ-094：显卡驱动/工具包环境炸掉时用户要有一条"先让我进得去"的路。
        // 探测本身已经是失败即降级的，这个开关额外把结论写成"已跳过"。
        let skip_cuda_check = std::env::args().any(|a| a == "--skip-cuda-check");
        std::thread::spawn(move || {
            let report = crate::ai::gpu::probe();
            let _ = std::fs::create_dir_all(&dir);
            if skip_cuda_check {
                let mut report = report;
                report.notes.push(
                    "启动参数 --skip-cuda-check 生效：本次未做 CUDA 工具包校验。".to_string(),
                );
                if let Err(e) = crate::ai::gpu::write_report_of(&dir, &report) {
                    eprintln!("写入 gpu_report.log 失败：{e}");
                }
                return;
            }
            if let Err(e) = crate::ai::gpu::write_report(&dir) {
                eprintln!("写入 gpu_report.log 失败：{e}");
            }
            eprintln!(
                "GPU: backend={} status={} tier={} — {}",
                report.backend, report.status, report.tier, report.reason
            );
        });
    }
    let shared = Arc::new(state);

    // Model loading is explicit: opening the app must not initialize a model.
    #[cfg(any())]
    {
        let st = Arc::clone(&shared);
        std::thread::spawn(move || {
            let trace = |msg: String| {
                let _ = std::fs::write(
                    st.data_dir.join("autoload.log"),
                    format!("{}\n{}", chrono::Local::now().format("%H:%M:%S"), msg),
                );
            };
            let cfg = st.cfg.read().clone();
            trace(format!(
                "thread start auto_load={} path={}",
                cfg.model.auto_load, cfg.model.embed_model
            ));
            // 便携版没写过配置时 embed_model 是空的。
            // 这里自动在 exe 同级 models/ 与 data/ 里找 .gguf，
            // 让「解压即用」成立，而不是逼用户先去设置页填路径。
            let mut cfg = cfg;
            if cfg.model.embed_model.trim().is_empty() {
                // 便携版/安装版首次运行：在 models\ 里自动挑一个能直接加载的嵌入模型。
                if let Some(found) = crate::models::autodetect_embed_model(
                    &cfg.model,
                    &st.app_dir,
                    &st.data_dir,
                ) {
                    cfg.model.embed_model = found;
                    let mut c = st.cfg.read().clone();
                    c.model.embed_model = cfg.model.embed_model.clone();
                    let _ = c.save(&crate::config::config_path(Some(&st.app_dir)));
                    *st.cfg.write() = c;
                    trace(format!("autodetect model={}", cfg.model.embed_model));
                }
            }
            if cfg.model.auto_load && !cfg.model.embed_model.trim().is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(600));
                match crate::ai::build_embedder_with_index(&cfg.model, &cfg.index, cfg.index.batch_size) {
                    Ok(emb) => {
                        let emb: Arc<dyn crate::ai::Embedder> = Arc::from(emb);
                        let dim = emb.dim();
                        let info = emb.info();
                        let shared_media = info.backend == "candle"
                            && info.modalities.iter().any(|m| matches!(m.as_str(), "image" | "audio" | "video"));
                        let media_dim = if shared_media { dim } else { 256 };
                        trace(format!("loaded dim={} backend={}", dim, info.backend));
                        *st.embedder.write() = Arc::clone(&emb);
                        let mut c = st.cfg.read().clone();
                        c.model.embedding_dim = dim;
                        let _ = c.save(&crate::config::config_path(Some(&st.app_dir)));
                        *st.cfg.write() = c;
                        if let Ok(v) = crate::vector::VectorStore::open(
                            &st.data_dir.join("vectors"),
                            dim,
                        ) {
                            // 与 model_load 同理：维度变化清空了旧向量，
                            // 但库里 embed_state 仍是 ok，必须标回 pending 才会重建。
                            if v.len() == 0 && st.db.count_state("embed", "ok").unwrap_or(0) > 0 {
                                let _ = st.db.reset_states("embed");
                                let _ = st.db.log(
                                    "model",
                                    "嵌入维度变化，已把全部文件标记为待重新向量化",
                                );
                            }
                            *st.vectors.write() = Arc::new(v);
                        }
                        let media_dir = st.data_dir.join("vectors_media");
                        let old_media_count = st.media_vectors.read().len();
                        if let Ok(v) = crate::vector::VectorStore::open(&media_dir, media_dim) {
                            if old_media_count > 0 && v.len() == 0 {
                                let _ = st.db.reset_states("embed");
                                let _ = st.db.log("model", "媒体向量空间已切换，已把文件标记为待重新向量化");
                            }
                            *st.media_vectors.write() = Arc::new(v);
                        }
                        let media_embedder: Arc<dyn crate::ai::Embedder> = if shared_media {
                            Arc::clone(&emb)
                        } else {
                            Arc::new(crate::ai::hash::HashEmbedder::new())
                        };
                        *st.media_embedder.write() = media_embedder;
                        let _ = st.rebuild_indexer();
                        // 刚重置过状态的话，立刻排队，用户无需手动点重建。
                        // The model is ready for search; pending files are
                        // indexed only after an explicit user action.
                        let _ = st.db.log(
                            "model",
                            &format!(
                                "自动加载 {}（{} 维 / {}）",
                                info.model, info.dim, info.device
                            ),
                        );
                    }
                    Err(e) => {
                        trace(format!("load failed: {e}"));
                        let _ = st.db.log("error", &format!("自动加载模型失败：{e}"));
                    }
                }
            }
        });
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            cmd::app_info,
            cmd::get_config,
            cmd::save_config,
            cmd::search,
            cmd::search_semantic,
            cmd::similar,
            cmd::stats,
            cmd::index_roots,
            cmd::recent,
            cmd::scan_start,
            cmd::scan_cancel,
            cmd::scan_progress,
            cmd::index_start,
            cmd::index_cancel,
            cmd::index_stats,
            cmd::model_info,
            cmd::gpu_devices,
            cmd::gpu_report,
            cmd::gpu_refresh_report,
            cmd::model_load,
            cmd::model_deactivate,
            cmd::model_list,
            cmd::model_dir_path,
            cmd::model_delete,
            cmd::model_import,
            cmd::model_import_folder,
            cmd::open_path,
            cmd::open_indexed_path,
            cmd::reveal_path,
            cmd::rename_path,
            cmd::ensure_preview,
            cmd::save_pdf_thumbnail,
            cmd::op_export,
            cmd::rules_list,
            cmd::rules_save,
            cmd::rules_apply,
            cmd::activity_list,
            cmd::graph_data,
            cmd::mind_map_data,
            cmd::tree_data,
            cmd::tree_browse,
            cmd::tree_search,
            star::star_drives,
            star::star_overview,
            star::star_children,
            star::star_semantic,
            star::star_layout_get,
            star::star_layout_put,
            cmd::workspace_add,
            cmd::workspace_state,
            cmd::workspace_remember,
            cmd::workspace_set_active,
            cmd::workspace_remove,
            cmd::workspace_set_startup,
            cmd::available_drives,
            cmd::watch_start,
            cmd::watch_stop,
            cmd::watch_status,
            cmd::server_start,
            cmd::server_stop,
            cmd::server_status,
            cmd::db_vacuum,
            cmd::db_clear,
            cmd::data_dir_path,
            cmd::extension_status,
            cmd::extension_install,
            cmd::extension_update,
            cmd::extension_remove,
        ])
        .setup(|app| {
            // 主窗口在 setup 里手动创建：`tauri.conf.json` 的 `dataDirectory`
            // 只接受「相对 localDataDir()」的路径，没法指到别的盘；而有些受限
            // 环境下 WebView2 把用户数据目录建在 C:\Users\...\AppData\Local
            // 里会直接报 0x800700AA（资源被占用）。这里用 WebviewWindowBuilder
            // 指定绝对路径，见 webview_data_dir 的候选顺序。
            create_main_window(app.handle())?;
            // 开发期打开 devtools
            #[cfg(debug_assertions)]
            {
                if let Some(w) = app.get_webview_window("main") {
                    w.open_devtools();
                }
            }
            schedule_startup_scan(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("DeepDark_Oracle 初始化失败")
        .run(|app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                use tauri::Manager;
                let state = app_handle.state::<Arc<state::AppState>>();
                if let Some(indexer) = state.indexer() {
                    indexer.stop();
                }
            }
        });
}

/// 创建主窗口（见 setup 里的说明）。
fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let conf = app.config().app.windows[0].clone();
    let mut builder = tauri::WebviewWindowBuilder::from_config(app, &conf)?;
    if let Some(dir) = webview_data_dir(app) {
        builder = builder.data_directory(dir);
    }
    builder.build()?;
    Ok(())
}

/// WebView2 用户数据目录的候选顺序：
/// 1. 环境变量 `WEBVIEW2_USER_DATA_FOLDER`（排障时手动覆盖）
/// 2. exe 同级 `webview-data`（便携版、安装到非系统盘时都在这里）
/// 3. 数据目录旁的 `webview`（data_dir 已回退到用户目录时）
/// 4. `%LOCALAPPDATA%\DeepDark_Oracle\webview`（最后兜底）
///
/// 非系统盘优先：本机实测 WebView2 数据目录落在 C: 盘 AppData 下会失败，
/// 换到其它盘立刻正常，而普通用户的机器上两种位置都能用。
fn webview_data_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER") {
        let p = std::path::PathBuf::from(p);
        let _ = std::fs::create_dir_all(&p);
        return Some(p);
    }
    let state = app.state::<Arc<state::AppState>>();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        candidates.push(dir.join("webview-data"));
    }
    candidates.push(state.data_dir.join("webview"));
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(local).join("DeepDark_Oracle").join("webview"));
    }
    let on_system_drive = |p: &std::path::Path| {
        p.to_string_lossy().to_ascii_lowercase().starts_with("c:")
    };
    let ordered = candidates
        .iter()
        .filter(|d| !on_system_drive(d))
        .chain(candidates.iter().filter(|d| on_system_drive(d)));
    for dir in ordered {
        if is_writable_dir(dir) {
            return Some(dir.clone());
        }
    }
    None
}

fn is_writable_dir(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
/// 启动时自动扫描（需求：自动扫描与文件变化提醒）。
///
/// 只做一次「增量扫描 + 变化统计」：扫描器本身按 mtime/size 判断新增、修改，
/// 并把扫描根下没再出现的记录标成 missing，所以不需要额外的 diff 逻辑。
/// 延迟 2.5 秒执行，避开窗口首帧与模型加载的高峰。
fn schedule_startup_scan(app: &tauri::AppHandle) {
    let cfg_now = {
        let state = app.state::<Arc<state::AppState>>();
        let current = state.cfg.read().clone();
        current
    };
    if !cfg_now.ui.auto_scan_on_start || cfg_now.scan.roots.is_empty() {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let state = handle.state::<Arc<state::AppState>>();
        // 用户可能刚在设置里关掉了开关，这里再读一次，避免关掉后仍然扫描。
        if !state.cfg.read().ui.auto_scan_on_start {
            return;
        }
        match cmd::start_scan(state.inner(), None, None, true) {
            Ok(()) => {
                let _ = state.db.log("scan", "启动时自动扫描已开始");
            }
            Err(e) => {
                // 扫描正在进行中属于正常情况（例如用户手动点了扫描），不记错误。
                if !e.contains("已在进行中") {
                    let _ = state.db.log("error", &format!("启动自动扫描失败：{e}"));
                }
            }
        }
    });
}
