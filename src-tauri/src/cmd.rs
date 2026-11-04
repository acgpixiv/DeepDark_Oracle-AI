//! Tauri 命令层：前端通过 invoke 调用。
use crate::config::AppConfig;
use crate::db::{Activity, FileRow, Rule};
use crate::db::{dir_lookup_key, fold_dir_children};
use crate::fileops;
use crate::search::{Hit, SearchQuery};
use crate::state::AppState;
use crate::util::human_size;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{Manager, State};

#[derive(Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub display_version: String,
    pub provider: String,
    pub files: i64,
    pub data_dir: String,
}

#[tauri::command]
pub fn app_info(state: State<'_, Arc<AppState>>) -> AppInfo {
    AppInfo {
        name: "DeepDark_Oracle".into(),
        version: crate::APP_VERSION.into(),
        display_version: crate::APP_DISPLAY_VERSION.into(),
        provider: state.embedder.read().info().backend,
        files: state.db.count().unwrap_or(0),
        data_dir: state.data_dir.to_string_lossy().to_string(),
    }
}

#[tauri::command]
pub fn get_config(state: State<'_, Arc<AppState>>) -> AppConfig {
    state.cfg.read().clone()
}

#[tauri::command]
pub fn save_config(
    state: State<'_, Arc<AppState>>,
    cfg: serde_json::Value,
) -> Result<(), String> {
    // 采用 JSON 深合并而不是整体覆盖：前端只改一个字段时
    // （例如只传 {"scan":{"roots":[...]}}）不能把模型路径等其他配置清空。
    let previous_cfg = state.cfg.read().clone();
    let mut merged = serde_json::to_value(previous_cfg.clone())
        .map_err(|e| format!("读取配置失败：{e}"))?;
    merge_json(&mut merged, &cfg);
    let mut new_cfg: AppConfig =
        serde_json::from_value(merged).map_err(|e| format!("配置格式错误：{e}"))?;
    normalize_workspace_config(&mut new_cfg.workspace);

    let autostart_before = state.cfg.read().ui.autostart;

    // 注意：save() 要的是**配置文件路径**，不是目录。
    // 传目录会得到 "拒绝访问 (os error 5)"，而界面只看到一句看不懂的错误。
    let path = crate::config::config_path(Some(&state.app_dir));
    new_cfg
        .save(&path)
        .map_err(|e| format!("保存配置失败：{e}"))?;
    // Removing a scan root also removes its indexed rows and all associated
    // vector slots. Still-configured nested roots remain protected.
    let removed_roots: Vec<String> = previous_cfg
        .scan
        .roots
        .iter()
        .filter(|old| {
            !new_cfg.scan.roots.iter().any(|current| {
                crate::util::norm_path(old).eq_ignore_ascii_case(&crate::util::norm_path(current))
            })
        })
        .cloned()
        .collect();
    for root in removed_roots {
        match state.db.remove_scan_root(&root, &new_cfg.scan.roots) {
            Ok(ids) => {
                for id in ids {
                    state.vectors.read().remove(id);
                    state.media_vectors.read().remove(id);
                    state.clip_vectors.read().remove(id);
                    state.audio_vectors.read().remove(id);
                }
                let _ = state.vectors.read().save();
                let _ = state.media_vectors.read().save();
                let _ = state.clip_vectors.read().save();
                let _ = state.audio_vectors.read().save();
                let _ = state.db.log("index", &format!("已移除扫描目录并清理索引数据：{root}"));
            }
            Err(error) => {
                let _ = state.db.log("error", &format!("清理已移除扫描目录失败 {root}: {error}"));
            }
        }
    }
    if autostart_before != new_cfg.ui.autostart {
        apply_autostart(new_cfg.ui.autostart, &state.app_dir);
    }
    *state.cfg.write() = new_cfg;
    if let Some(indexer) = state.indexer() {
        let cfg = state.cfg.read();
        indexer.set_batch_size(crate::ai::effective_batch_size(&cfg.model, cfg.index.batch_size));
        indexer.set_worker_threads(crate::ai::effective_parallel_threads(&cfg.model));
    }
    Ok(())
}

fn normalize_workspace_config(workspace: &mut crate::config::WorkspaceConfig) {
    fn normalize_paths(paths: &mut Vec<String>, limit: Option<usize>) {
        let mut seen = std::collections::HashSet::new();
        let mut normalized = Vec::with_capacity(paths.len());
        for path in paths.drain(..) {
            let path = crate::util::norm_path(&path);
            if !path.is_empty() && seen.insert(path.to_ascii_lowercase()) {
                normalized.push(path);
            }
        }
        if let Some(limit) = limit {
            normalized.truncate(limit);
        }
        *paths = normalized;
    }
    normalize_paths(&mut workspace.recent_paths, Some(3));
    normalize_paths(&mut workspace.pinned_paths, None);
    if !matches!(workspace.startup_scope.as_str(), "remember" | "all" | "indexed" | "fixed") {
        workspace.startup_scope = "remember".into();
    }
    if !matches!(workspace.last_scope.as_str(), "all" | "indexed" | "path") {
        workspace.last_scope = "indexed".into();
    }
    workspace.startup_path = crate::util::norm_path(&workspace.startup_path);
    workspace.last_path = crate::util::norm_path(&workspace.last_path);
}

/// 把「开机自启」写进 HKCU 的 Run 键（或从中删除）。
///
/// 用 `reg.exe` 而不是引入 winreg 依赖：这里只需要一次简单写入，
/// 且失败不影响配置保存（受限会话里注册表可能不可写，属于可接受降级）。
fn apply_autostart(enabled: bool, app_dir: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        const VALUE: &str = "DeepDark_Oracle";

        let mut cmd = std::process::Command::new("reg.exe");
        if enabled {
            let exe = app_dir.join("DeepDark_Oracle.exe");
            if !exe.is_file() {
                return;
            }
            let value = format!("\"{}\"", exe.display());
            cmd.args([
                "add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &value, "/f",
            ]);
        } else {
            cmd.args(["delete", RUN_KEY, "/v", VALUE, "/f"]);
        }
        let _ = cmd.creation_flags(CREATE_NO_WINDOW).output();
    }
    #[cfg(not(windows))]
    {
        let _ = (enabled, app_dir);
    }
}

/// 递归深合并：`src` 里的对象逐键并入 `dst`，数组与标量直接覆盖。
fn merge_json(dst: &mut serde_json::Value, src: &serde_json::Value) {
    match (dst, src) {
        (serde_json::Value::Object(d), serde_json::Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(slot) => merge_json(slot, v),
                    None => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (d, s) => *d = s.clone(),
    }
}

#[tauri::command]
pub async fn search(app: tauri::AppHandle, q: SearchQuery) -> Vec<Hit> {
    let state = Arc::clone(app.state::<Arc<AppState>>().inner());
    tauri::async_runtime::spawn_blocking(move || search_inner(&state, q))
        .await
        .unwrap_or_default()
}

fn search_inner(state: &Arc<AppState>, mut q: SearchQuery) -> Vec<Hit> {
    let query_has_cjk = q.text.chars().any(|character| {
        matches!(character as u32,
            0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x2fa1f)
    });
    let hits = state.search(&mut q);
    // Chinese queries cannot match English-only text through FTS5. Use the
    // multilingual embedding space for CJK input even when Chinese filename
    // matches exist, so those lexical hits do not hide English references.
    // Other languages only pay the semantic cost when lexical search is empty.
    if (!hits.is_empty() && !query_has_cjk) || q.text.trim().is_empty() {
        return hits;
    }

    // Keep the fast FTS5 path as the first attempt. When it cannot find a
    // lexical match, wake the configured embedding model on demand so a
    // multilingual query such as Chinese "发动机" can still recall an
    // English document (and image-only packages can use their media vectors).
    // A missing/unloadable model is deliberately non-fatal: the caller still
    // receives the already computed lexical result (normally an empty list).
    match semantic_search_inner(state, q) {
        Ok(semantic_hits) => semantic_hits,
        Err(error) => {
            let _ = state
                .db
                .log("error", &format!("按需语义检索未能启用：{error}"));
            hits
        }
    }
}

#[tauri::command]
pub async fn search_semantic(app: tauri::AppHandle, q: SearchQuery) -> Result<Vec<Hit>, String> {
    let state = Arc::clone(app.state::<Arc<AppState>>().inner());
    tauri::async_runtime::spawn_blocking(move || semantic_search_inner(&state, q))
        .await
        .map_err(|error| format!("语义检索任务失败：{error}"))?
}

fn semantic_search_inner(state: &Arc<AppState>, mut q: SearchQuery) -> Result<Vec<Hit>, String> {
    let text = q.text.clone();
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    // The normal search command stays model-free. An explicit semantic request
    // is the point at which we lazily restore the last selected model, so a
    // fresh application launch does not spend time or VRAM initializing AI.
    if state.embedder.read().info().backend == "hash" {
        let cfg = state.cfg.read().clone();
        let previous = if cfg.model.embed_model.trim().is_empty() {
            cfg.model.last_embed_model.clone()
        } else {
            cfg.model.embed_model.clone()
        };
        if previous.trim().is_empty() {
            return Err("未配置可用于语义检索的嵌入模型".into());
        }
        model_load_inner(state, "auto".into(), Some(previous))?;
    }
    let embedder = state.embedder.read();
    let info = embedder.info();
    let vec = embedder.embed_text(&text, true).map_err(|e| e.to_string())?;
    q.query_vector = Some(vec);
    if info
        .modalities
        .iter()
        .any(|m| matches!(m.as_str(), "image" | "audio" | "video"))
    {
        q.vector_space = Some("shared".into());
    }
    q.use_semantic = Some(true);
    Ok(state.search(&mut q))
}

#[tauri::command]
pub async fn similar(app: tauri::AppHandle, file_id: i64, kind: Option<String>, top: Option<usize>) -> Vec<Hit> {
    let state = Arc::clone(app.state::<Arc<AppState>>().inner());
    tauri::async_runtime::spawn_blocking(move || similar_inner(&state, file_id, kind, top))
        .await
        .unwrap_or_default()
}

fn similar_inner(state: &Arc<AppState>, file_id: i64, kind: Option<String>, top: Option<usize>) -> Vec<Hit> {
    let vecs = Arc::clone(&state.vectors.read());
    let media = Arc::clone(&state.media_vectors.read());
    let clip = Arc::clone(&state.clip_vectors.read());
    let audio = Arc::clone(&state.audio_vectors.read());
    let scan = state.cfg.read().scan.clone();
    let enabled_roots: Vec<String> = scan.roots.iter()
        .filter(|root| !scan.disabled_roots.iter().any(|disabled| {
            crate::util::norm_path(disabled).eq_ignore_ascii_case(&crate::util::norm_path(root))
        }))
        .cloned()
        .collect();
    crate::search::similar_scoped(
        &state.db,
        &vecs,
        &media,
        &clip,
        &audio,
        file_id,
        kind.as_deref(),
        top.unwrap_or(30),
        if scan.roots.is_empty() { None } else { Some(&enabled_roots) },
    )
}

#[derive(Serialize)]
pub struct KindStat {
    pub kind: String,
    pub label: String,
    pub count: i64,
    pub bytes: i64,
}

#[derive(Serialize)]
pub struct StatsOut {
    pub files: i64,
    pub bytes: i64,
    pub bytes_label: String,
    pub kinds: Vec<KindStat>,
    pub text_ok: i64,
    pub embed_ok: i64,
    pub missing: i64,
    pub db_bytes: i64,
    pub vector_count: i64,
    pub dim: i64,
}

#[derive(Serialize)]
pub struct IndexRootOut {
    pub root: String,
    pub enabled: bool,
    pub last_scan: i64,
    pub index_progress: f32,
    pub kinds_label: String,
    pub files: i64,
    pub bytes: i64,
    pub discovered_files: i64,
    pub discovered_bytes: i64,
    pub bytes_label: String,
    pub added: i64,
    pub updated: i64,
    pub skipped: i64,
    pub errors: i64,
    pub missing: i64,
    pub text_ok: i64,
    pub embed_ok: i64,
    pub completed: i64,
    pub text_pending: i64,
    pub embed_pending: i64,
    pub kinds: Vec<IndexKindStatOut>,
    pub last_error: String,
}

#[derive(Serialize)]
pub struct IndexKindStatOut {
    pub kind: String,
    pub label: String,
    pub count: i64,
    pub bytes: i64,
}

#[tauri::command]
pub fn index_roots(state: State<'_, Arc<AppState>>) -> Vec<IndexRootOut> {
    let scan = state.cfg.read().scan.clone();
    let _ = state.db.sync_index_roots(&scan.roots, &scan.disabled_roots);
    state.db.index_roots().unwrap_or_default().into_iter().map(|r| IndexRootOut {
        index_progress: if r.files > 0 {
            (r.completed as f32 * 100.0 / r.files as f32).clamp(0.0, 100.0)
        } else { 0.0 },
        kinds_label: r.kinds.iter().map(|k| format!("{} {}", crate::util::kind_label(&k.kind), k.count)).collect::<Vec<_>>().join(" / "),
        root: r.root,
        enabled: r.enabled,
        last_scan: r.last_scan,
        files: r.files,
        bytes: r.bytes,
        discovered_files: r.discovered_files,
        discovered_bytes: r.discovered_bytes,
        bytes_label: human_size(r.bytes),
        added: r.added,
        updated: r.updated,
        skipped: r.skipped,
        errors: r.errors,
        missing: r.missing,
        text_ok: r.text_ok,
        embed_ok: r.embed_ok,
        completed: r.completed,
        text_pending: r.text_pending,
        embed_pending: r.embed_pending,
        last_error: r.last_error,
        kinds: r.kinds.into_iter().map(|k| IndexKindStatOut {
            label: crate::util::kind_label(&k.kind).to_string(),
            kind: k.kind,
            count: k.count,
            bytes: k.bytes,
        }).collect(),
    }).collect()
}

#[tauri::command]
pub fn stats(state: State<'_, Arc<AppState>>) -> StatsOut {
    let db = &state.db;
    let files = db.count().unwrap_or(0);
    let bytes = db.total_bytes().unwrap_or(0);
    let kinds = db
        .kind_stats()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, c, b)| KindStat {
            label: crate::util::kind_label(&k).to_string(),
            kind: k,
            count: c,
            bytes: b,
        })
        .collect();
    let missing = db
        .query_rows("SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags FROM files WHERE missing=1", vec![])
        .len() as i64;
    let db_bytes = std::fs::metadata(state.data_dir.join("index.db"))
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let store = state.vectors.read();
    StatsOut {
        files,
        bytes,
        bytes_label: human_size(bytes),
        kinds,
        text_ok: db.count_state("text", "ok").unwrap_or(0),
        embed_ok: db.count_state("embed", "ok").unwrap_or(0),
        missing,
        db_bytes,
        vector_count: store.len() as i64,
        dim: store.dim() as i64,
    }
}

#[tauri::command]
pub fn recent(state: State<'_, Arc<AppState>>, limit: Option<usize>) -> Vec<Hit> {
    let mut q = SearchQuery::default();
    q.limit = Some(limit.unwrap_or(40));
    q.sort = Some("mtime".into());
    q.desc = true;
    state.search(&mut q)
}

// ---------------------------------------------------------------- 扫描
#[derive(Serialize, Clone)]
pub struct ScanProgress {
    pub running: bool,
    pub dirs: u64,
    pub files: u64,
    pub added: u64,
    pub updated: u64,
    pub skipped: u64,
    pub errors: u64,
    pub missing: u64,
    pub bytes: u64,
    pub discovered_files: u64,
    pub discovered_bytes: u64,
    pub current: String,
    pub seconds: f64,
    pub done: bool,
    pub cancelled: bool,
    pub extensions: Vec<crate::scan::ExtensionScanStats>,
    pub roots: Vec<crate::scan::RootScanStats>,
}

/// 扫描 + 增量索引的共用入口。
///
/// 之所以把 scan_start 拆成「命令壳 + 这个函数」，是因为启动时的自动扫描
/// （需求：启动时自动扫描）没有前端 State，只能在 lib.rs 里直接调。
pub fn start_scan(
    state: &Arc<AppState>,
    roots: Option<Vec<String>>,
    index_what: Option<String>,
    scan_only: bool,
) -> Result<(), String> {
    if state.scan_busy.load(Ordering::Relaxed) {
        return Err("扫描已在进行中".into());
    }
    let cfg = state.cfg.read().clone();
    let mut sc = cfg.scan.clone();
    let requested: Option<std::collections::HashSet<String>> = roots.map(|items| {
        items.into_iter().map(|root| crate::util::norm_path(&root).to_ascii_lowercase()).collect()
    });
    sc.roots.retain(|root| {
        let normalized = crate::util::norm_path(root);
        let enabled = !sc.disabled_roots.iter().any(|disabled| {
            crate::util::norm_path(disabled).eq_ignore_ascii_case(&normalized)
        });
        enabled && requested.as_ref().map(|set| set.contains(&normalized.to_ascii_lowercase())).unwrap_or(true)
    });
    if sc.roots.is_empty() {
        return Err("请先选择要扫描的目录".into());
    }
    let needs_embedding = match index_what.as_deref() {
        Some("text") => false,
        Some("embed") | Some("auto") => true,
        _ => cfg.index.auto_embed,
    };
    if !scan_only && needs_embedding {
        ensure_index_model(state)?;
    }
    let _ = state.db.sync_index_roots(&cfg.scan.roots, &cfg.scan.disabled_roots);
    state.scan_cancel.store(false, Ordering::Relaxed);
    state.scan_busy.store(true, Ordering::Relaxed);
    let db = Arc::clone(&state.db);
    let app_state = Arc::clone(state);
    let cancel = Arc::clone(&state.scan_cancel);
    let busy = Arc::clone(&state.scan_busy);
    let stats_shared = Arc::clone(&state.scan_stats);
    let auto_text = cfg.index.auto_fulltext;
    let auto_embed = cfg.index.auto_embed;
    let requested_index_what = index_what;
    std::thread::spawn(move || {
        let scanner = crate::scan::Scanner::with_shared(Arc::clone(&stats_shared), Arc::clone(&cancel));
        let stats = scanner.run(&db, &sc, |_| {});
        let scan_ts = crate::util::now_ts();
        for root in &stats.roots {
            let _ = db.upsert_index_root(
                &root.root,
                scan_ts,
                root.files as i64,
                root.bytes as i64,
                root.added as i64,
                root.updated as i64,
                root.skipped as i64,
                root.errors as i64,
                root.missing as i64,
                root.discovered_files as i64,
                root.discovered_bytes as i64,
                &root.last_error,
            );
            if root.errors > 0 {
                let _ = db.log("error", &format!("目录扫描 {}：{}；错误 {} 项：{}", root.root, if stats.cancelled { "已中断" } else { "完成" }, root.errors, root.last_error));
            } else {
                let _ = db.log("scan", &format!("目录扫描 {}：{} 个文件，新增 {}，更新 {}，跳过 {}，缺失 {}", root.root, root.files, root.added, root.updated, root.skipped, root.missing));
            }
        }
        if !stats.cancelled && !scan_only {
            let indexer = match app_state.indexer() {
                Some(i) => i,
                None => app_state.rebuild_indexer(),
            };
            let what = requested_index_what.as_deref().or(match (auto_text, auto_embed) {
                (true, true) => Some("auto"),
                (true, false) => Some("text"),
                (false, true) => Some("embed"),
                (false, false) => None,
            });
            if let Some(what) = what {
                indexer.set_progress_scope(Some(sc.roots.clone()));
                match db.pending_ids_in_roots(what, 100_000, &sc.roots) {
                    Ok(ids) => {
                        if !ids.is_empty() {
                            indexer.enqueue(ids, what);
                            if cancel.load(Ordering::Relaxed) {
                                indexer.cancel_pending();
                            }
                        }
                    }
                    Err(error) => {
                        let _ = db.log("error", &format!("查询待索引文件失败：{error}"));
                    }
                }
            }
        }
        busy.store(false, Ordering::Relaxed);
    });
    Ok(())
}

#[tauri::command]
pub fn scan_start(
    state: State<'_, Arc<AppState>>,
    roots: Option<Vec<String>>,
    index_what: Option<String>,
    scan_only: Option<bool>,
) -> Result<(), String> {
    start_scan(state.inner(), roots, index_what, scan_only.unwrap_or(false))
}

#[tauri::command]
pub fn scan_cancel(state: State<'_, Arc<AppState>>) {
    state.scan_cancel.store(true, Ordering::Relaxed);
    if let Some(indexer) = state.indexer() {
        indexer.cancel_pending();
    }
}

#[tauri::command]
pub fn scan_progress(state: State<'_, Arc<AppState>>) -> ScanProgress {
    let s = state.scan_stats.snapshot();
    ScanProgress {
        running: state.scan_busy.load(Ordering::Relaxed),
        dirs: s.dirs,
        files: s.files,
        added: s.added,
        updated: s.updated,
        skipped: s.skipped,
        errors: s.errors,
        missing: s.missing,
        bytes: s.bytes,
        discovered_files: s.discovered_files,
        discovered_bytes: s.discovered_bytes,
        current: s.current,
        seconds: s.seconds,
        done: s.done,
        cancelled: s.cancelled,
        extensions: s.extensions,
        roots: s.roots,
    }
}

// ---------------------------------------------------------------- 索引
#[tauri::command]
pub fn index_start(
    state: State<'_, Arc<AppState>>,
    what: Option<String>,
    force: Option<bool>,
    roots: Option<Vec<String>>,
) -> Result<usize, String> {
    let idx = match state.indexer() {
        Some(i) => i,
        None => state.rebuild_indexer(),
    };
    let what = what.unwrap_or_else(|| "auto".into());
    let scan = state.cfg.read().scan.clone();
    let configured = !scan.roots.is_empty();
    let enabled: Vec<String> = scan.roots.iter()
        .filter(|root| !scan.disabled_roots.iter().any(|disabled| {
            crate::util::norm_path(disabled).eq_ignore_ascii_case(&crate::util::norm_path(root))
        }))
        .cloned()
        .collect();
    let requested = roots.unwrap_or_else(|| enabled.clone());
    let scope: Vec<String> = if configured {
        requested.into_iter().filter(|root| enabled.iter().any(|active| {
            crate::util::norm_path(active).eq_ignore_ascii_case(&crate::util::norm_path(root))
        })).collect()
    } else {
        requested
    };
    if configured && scope.is_empty() {
        return Ok(0);
    }
    idx.set_progress_scope(if configured { Some(scope.clone()) } else { None });
    // force=true 时把所有文件标回 pending —— 换模型（维度变了）后
    // 旧向量会被清空，但 embed_state 还是 ok，不加这一步就永远重建不了。
    if force.unwrap_or(false) {
        idx.reset_progress();
        if configured {
            let _ = state.db.reset_states_in_roots(&what, &scope);
        } else {
            let _ = state.db.reset_states(&what);
        }
    }
    let ids = if configured {
        state.db.pending_ids_in_roots(&what, 100_000, &scope).map_err(|e| e.to_string())?
    } else {
        state.db.pending_ids(&what, 100_000).map_err(|e| e.to_string())?
    };
    let n = ids.len();
    if n > 0 {
        idx.enqueue(ids, &what);
    }
    Ok(n)
}

#[tauri::command]
pub fn index_cancel(state: State<'_, Arc<AppState>>) {
    if let Some(indexer) = state.indexer() {
        indexer.cancel_pending();
    }
}

#[tauri::command]
pub fn index_stats(state: State<'_, Arc<AppState>>) -> crate::index::IndexStats {
    match state.indexer() {
        Some(i) => i.snapshot(),
        None => crate::index::IndexStats::default(),
    }
}

// ---------------------------------------------------------------- 模型
#[derive(Serialize)]
pub struct ModelInfoOut {
    pub backend: String,
    pub model: String,
    pub dim: usize,
    pub device: String,
    pub modalities: Vec<String>,
    pub ready: bool,
    pub detail: String,
    /// 跨模态模型状态：图片（中文 CLIP）/ 声音（CLAP），没装就是空字符串
    pub clip: String,
    pub clap: String,
}

#[derive(Serialize)]
pub struct GpuDeviceOut {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub total_memory: u64,
    pub free_memory: u64,
    pub integrated: bool,
}

#[tauri::command]
pub fn gpu_devices() -> Vec<GpuDeviceOut> {
    #[cfg(feature = "llama")]
    {
        return crate::ai::llama::gpu_devices()
            .into_iter()
            .map(|d| GpuDeviceOut {
                id: d.id,
                name: d.name,
                backend: d.backend,
                total_memory: d.total_memory,
                free_memory: d.free_memory,
                integrated: d.integrated,
            })
            .collect();
    }
    #[cfg(not(feature = "llama"))]
    Vec::new()
}

/// 完整的 GPU 能力报告（REQ-093）。前端「显示适配 / 兼容性诊断」页用它。
#[tauri::command]
pub fn gpu_report() -> crate::ai::gpu::GpuReport {
    crate::ai::gpu::probe()
}

/// 重新探测并落盘 `gpu_report.log`，返回写入路径。
///
/// 「重新检测」按钮走这条：用户插拔外置显卡或装完驱动后，不必重启程序。
#[tauri::command]
pub fn gpu_refresh_report(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let dir = state.data_dir.clone();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let path =
        crate::ai::gpu::write_report(&dir).map_err(|e| format!("写入 gpu_report.log 失败：{e}"))?;
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
pub fn model_info(state: State<'_, Arc<AppState>>) -> ModelInfoOut {
    let i = state.embedder.read().info();
    let clip = state.clip.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    let clap = state.clap.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    ModelInfoOut {
        backend: i.backend,
        model: i.model,
        dim: i.dim,
        device: i.device,
        modalities: i.modalities,
        ready: i.ready,
        detail: i.detail,
        clip,
        clap,
    }
}

#[tauri::command]
pub fn model_load(state: State<'_, Arc<AppState>>, backend: String, path: Option<String>) -> Result<ModelInfoOut, String> {
    model_load_inner(state.inner(), backend, path)
}

/// Load an embedding model for an explicit UI/indexing request. Keeping this
/// separate from the Tauri wrapper lets the indexer perform a lazy reload
/// without pretending that opening the application needs a model.
pub fn model_load_inner(state: &Arc<AppState>, backend: String, path: Option<String>) -> Result<ModelInfoOut, String> {
    let mut cfg = state.cfg.read().clone();
    // The frontend sends a model path, never an implementation choice. Keep
    // the old backend argument as a compatibility alias for older clients,
    // but select the adapter from the path and its contents here.
    let requested = path.unwrap_or_else(|| cfg.model.embed_model.clone()).trim().to_string();
    let resolved = if backend.eq_ignore_ascii_case("hash") || requested.is_empty() {
        None
    } else {
        Some(crate::ai::resolve_model_path(&requested)?)
    };
    let resolved_s = resolved.as_ref().map(|p| p.to_string_lossy().to_string());
    let mode = if backend.eq_ignore_ascii_case("hash") || requested.is_empty() {
        "hash"
    } else if resolved.as_ref().map(|p| p.is_dir()).unwrap_or(false)
        || resolved.as_ref().and_then(|p| p.extension()).map(|x| x.eq_ignore_ascii_case("safetensors")).unwrap_or(false)
    {
        "candle"
    } else if resolved.as_ref().and_then(|p| p.extension()).map(|x| x.eq_ignore_ascii_case("gguf")).unwrap_or(false) {
        "llama"
    } else {
        return Err(format!("无法识别模型格式：{}（需要 GGUF 或 safetensors）", requested));
    };
    cfg.model.embed_model = if mode == "hash" { String::new() } else { resolved_s.clone().unwrap_or(requested.clone()) };
    if mode != "hash" {
        cfg.model.last_embed_model = cfg.model.embed_model.clone();
    }
    let emb: Arc<dyn crate::ai::Embedder> = match mode {
        "llama" => {
            #[cfg(feature = "llama")]
            {
                let e = crate::ai::llama::LlamaEmbedder::load_with_media_config(
                    resolved_s.as_deref().unwrap_or(&requested),
                    cfg.model.context_size as i32,
                    crate::ai::effective_parallel_threads(&cfg.model)
                        .min(i32::MAX as usize) as i32,
                    &cfg.model.gpu_device,
                    cfg.index.video_token_budget,
                    cfg.index.audio_seconds,
                    cfg.index.video_frame_interval,
                )?;
                Arc::new(e)
            }
            #[cfg(not(feature = "llama"))]
            {
                return Err("当前构建未启用 llama.cpp（用 --features llama 重新编译）".into());
            }
        }
        "candle" => {
            #[cfg(feature = "candle")]
            {
                let model_path = std::path::Path::new(resolved_s.as_deref().unwrap_or(&requested));
                let dir = if model_path.is_dir() {
                    model_path.to_path_buf()
                } else {
                    model_path
                        .parent()
                        .ok_or_else(|| "safetensors 模型路径没有父目录".to_string())?
                        .to_path_buf()
                };
                if !crate::ai::is_embedding_gemma_dir(&dir) {
                    return Err(format!(
                        "当前原生 Candle 适配器只支持 EmbeddingGemma2 safetensors：{}；Qwen3-Embedding/Jina v5 Omni 请导入对应 GGUF，或安装 Python/ONNX 扩展",
                        dir.display()
                    ));
                }
                let e = crate::ai::candle_backend::CandleEmbedder::load(
                    &dir,
                    &cfg.model.gpu_device,
                    crate::ai::effective_batch_size(&cfg.model, cfg.index.batch_size),
                    cfg.model.context_size as usize,
                    cfg.index.image_token_budget,
                    cfg.index.video_token_budget,
                    cfg.index.vision_encoder,
                    cfg.index.audio_encoder,
                    cfg.index.video_index_audio,
                    cfg.index.audio_seconds,
                    cfg.index.video_frame_interval,
                    cfg.index.video_max_frames,
                    cfg.index.image_max_side,
                    cfg.index.pdf_image_token_budget,
                )?;
                Arc::new(e)
            }
            #[cfg(not(feature = "candle"))]
            {
                return Err("当前构建未启用 Candle；请使用包含 candle 的构建版本".into());
            }
        }
        _ => Arc::new(crate::ai::hash::HashEmbedder::new()),
    };
    let info = emb.info();
    let dim = emb.dim();
    // 需求 #7（GPU 利用率）：原来这里写死 `info.backend == "candle"`，于是任何
    // 走 llama.cpp(GGUF) 的多模态模型都会被判成「不支持共享媒体空间」，图片全部
    // 退回 256 维 HashEmbedder（纯 CPU 哈希），GGUF 自带的视觉塔一次都不跑 ——
    // 这就是「索引时 GPU 占用几乎为零」的结构性原因之一。
    // 判据应该是**模型自报有没有 media 模态**，与它跑在哪个后端无关：
    //   · candle + image  → 共享（原有行为不变）
    //   · llama  + image  → 共享（本次修复，需模型同目录带 mmproj 且加载成功）
    //   · 纯文本模型      → 仍走 256 维 hash 媒体空间（保持旧库兼容）
    let shared_media = info.modalities.iter().any(|m| matches!(m.as_str(), "image" | "audio" | "video"));
    let media_dim = if shared_media { dim } else { 256 };
    let media_embedder: Arc<dyn crate::ai::Embedder> = if shared_media {
        Arc::clone(&emb)
    } else {
        Arc::new(crate::ai::hash::HashEmbedder::new())
    };
    let _ = cfg.save(&crate::config::config_path(Some(&state.app_dir)));
    *state.cfg.write() = cfg;
    // Cross-modal encoders are opt-in and are loaded only after the user
    // explicitly chooses a model or starts indexing.
    let (clip, clap) = crate::state::load_cross_modal(
        &state.cfg.read().model,
        &state.app_dir,
        &state.data_dir,
    );
    *state.clip.write() = clip;
    *state.clap.write() = clap;
    *state.embedder.write() = emb;
    // 维度变了，向量库需要重建
    let vec_dir = state.data_dir.join("vectors");
    let old_count = state.vectors.read().len();
    if let Ok(v) = crate::vector::VectorStore::open(&vec_dir, dim) {
        // 换模型后维度变化会让旧的向量文件被丢弃。此时 embed_state 还是 ok，
        // 不重置就会「重建索引排队 0 个」——用户以为重建了，其实一条都没跑。
        let new_count = v.len();
        if old_count > 0 && new_count == 0 {
            let _ = state.db.reset_states("embed");
            let _ = state.db.log("model", "嵌入维度变化，已把全部文件标记为待重新向量化");
        }
        *state.vectors.write() = Arc::new(v);
    }
    let media_dir = state.data_dir.join("vectors_media");
    let old_media_count = state.media_vectors.read().len();
    if let Ok(v) = crate::vector::VectorStore::open(&media_dir, media_dim) {
        if old_media_count > 0 && v.len() == 0 {
            let _ = state.db.reset_states("embed");
            let _ = state.db.log("model", "媒体向量空间已切换，已把文件标记为待重新向量化");
        }
        *state.media_vectors.write() = Arc::new(v);
    }
    *state.media_embedder.write() = media_embedder;
    let _ = state.rebuild_indexer();
    let i = state.embedder.read().info();
    let clip = state.clip.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    let clap = state.clap.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    Ok(ModelInfoOut {
        backend: i.backend,
        model: i.model,
        dim: i.dim,
        device: i.device,
        modalities: i.modalities,
        ready: i.ready,
        detail: i.detail,
        clip,
        clap,
    })
}

/// Ensure an indexing run has the explicitly configured embedding backend.
/// Search keeps the hash baseline until this function is reached, so opening
/// the application never allocates model memory. A deactivated model falls
/// back to `last_embed_model` for the next deliberate indexing request.
pub fn ensure_index_model(state: &Arc<AppState>) -> Result<(), String> {
    let cfg = state.cfg.read().clone();
    let path = if !cfg.model.embed_model.trim().is_empty() {
        cfg.model.embed_model.trim().to_string()
    } else {
        cfg.model.last_embed_model.trim().to_string()
    };
    if path.is_empty() {
        return Ok(());
    }
    let active = state.embedder.read().info();
    if active.ready && active.backend != "hash" {
        return Ok(());
    }
    model_load_inner(state, "auto".into(), Some(path)).map(|_| ())
}

// ---------------------------------------------------------------- 模型管理
/// Disable the active embedding model and restore the deterministic hash
/// baseline. Clearing the persisted path is part of the operation; otherwise
/// the previous model remains marked as active after the next refresh.
#[tauri::command]
pub fn model_deactivate(state: State<'_, Arc<AppState>>) -> Result<ModelInfoOut, String> {
    let mut cfg = state.cfg.read().clone();
    if !cfg.model.embed_model.trim().is_empty() {
        cfg.model.last_embed_model = cfg.model.embed_model.clone();
    }
    cfg.model.embed_model.clear();
    cfg.model.embedding_dim = 256;

    let emb: Arc<dyn crate::ai::Embedder> = Arc::new(crate::ai::hash::HashEmbedder::new());
    let media_emb: Arc<dyn crate::ai::Embedder> = Arc::new(crate::ai::hash::HashEmbedder::new());
    let old_count = state.vectors.read().len();
    let vectors = crate::vector::VectorStore::open(&state.data_dir.join("vectors"), 256)
        .map_err(|e| format!("打开基础向量库失败: {e}"))?;
    let mut needs_reindex = old_count > 0 && vectors.is_empty();
    if needs_reindex {
        let _ = state.db.reset_states("embed");
        let _ = state.db.log("model", "已停用嵌入模型，等待重新建立基础向量");
    }
    let old_media_count = state.media_vectors.read().len();
    let media_vectors = crate::vector::VectorStore::open(&state.data_dir.join("vectors_media"), 256)
        .map_err(|e| format!("打开媒体向量库失败: {e}"))?;
    if old_media_count > 0 && media_vectors.is_empty() {
        let _ = state.db.reset_states("embed");
        needs_reindex = true;
    }

    cfg.save(&crate::config::config_path(Some(&state.app_dir)))
        .map_err(|e| format!("保存模型配置失败: {e}"))?;
    *state.cfg.write() = cfg;
    *state.embedder.write() = emb;
    *state.media_embedder.write() = media_emb;
    // Deactivation must also remove any CLIP/CLAP instances loaded for the
    // previous model. Otherwise the settings page can report stale media
    // capability and indexing can keep using those encoders.
    *state.clip.write() = None;
    *state.clap.write() = None;
    *state.vectors.write() = Arc::new(vectors);
    *state.media_vectors.write() = Arc::new(media_vectors);
    let _ = state.rebuild_indexer();
    if needs_reindex {
        let _ = state.db.log("model", "模型已切换；待处理向量会在你手动开始索引后重建");
    }

    let i = state.embedder.read().info();
    let clip = state.clip.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    let clap = state.clap.read().as_ref().map(|m| m.describe()).unwrap_or_default();
    Ok(ModelInfoOut {
        backend: i.backend,
        model: i.model,
        dim: i.dim,
        device: i.device,
        modalities: i.modalities,
        ready: i.ready,
        detail: i.detail,
        clip,
        clap,
    })
}

#[tauri::command]
pub fn model_list(state: State<'_, Arc<AppState>>) -> Vec<crate::models::ModelEntry> {
    let cfg = state.cfg.read().model.clone();
    let dir = crate::models::model_dir(&state.app_dir, &state.data_dir);
    crate::models::scan(&cfg, &[dir])
}

#[tauri::command]
pub fn model_dir_path(state: State<'_, Arc<AppState>>) -> String {
    crate::models::model_dir(&state.app_dir, &state.data_dir)
        .to_string_lossy()
        .to_string()
}

#[tauri::command]
pub fn model_delete(state: State<'_, Arc<AppState>>, path: String) -> Result<(), String> {
    let cfg = state.cfg.read().model.clone();
    crate::models::remove(&path, &cfg)?;
    let _ = state.db.log("model", &format!("已删除模型文件：{path}"));
    Ok(())
}

#[tauri::command]
pub fn model_import(
    state: State<'_, Arc<AppState>>,
    src: String,
) -> Result<String, String> {
    let dir = crate::models::model_dir(&state.app_dir, &state.data_dir);
    let dest = crate::models::import(&src, &dir)?;
    let _ = state.db.log("model", &format!("已导入模型：{dest}"));
    Ok(dest)
}

/// 导入一个模型文件夹，自动识别 GGUF / safetensors 及 tokenizer、配置等配套文件。
#[tauri::command]
pub fn model_import_folder(
    state: State<'_, Arc<AppState>>,
    src: String,
) -> Result<crate::models::ModelImportResult, String> {
    let dir = crate::models::model_dir(&state.app_dir, &state.data_dir);
    let result = crate::models::import_folder(&src, &dir)?;
    let msg = format!("已导入模型目录：{}（{} 个文件，{}）", result.destination, result.copied_files, crate::util::human_size(result.total_bytes));
    let _ = state.db.log("model", &msg);
    Ok(result)
}

// ---------------------------------------------------------------- 文件操作
#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    open::that(&path).map_err(|e| e.to_string())
}

/// Open a result only after checking that its id still refers to the path
/// shown by the UI. This prevents a stale result card from opening a different
/// file after an incremental rescan or rename.
#[tauri::command]
pub fn open_indexed_path(
    state: State<'_, Arc<AppState>>,
    file_id: i64,
    path: String,
) -> Result<(), String> {
    if file_id <= 0 {
        return open_path(path);
    }
    let expected = crate::util::norm_path(&path);
    let row = state
        .db
        .get_by_id(file_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "检索结果已失效，请重新搜索".to_string())?;
    if crate::util::norm_path(&row.path) != expected {
        return Err("检索结果对应的文件已变化，请重新搜索".into());
    }
    open_path(row.path)
}

#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path))
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let p = Path::new(&path);
        open::that(p.parent().unwrap_or(p)).map_err(|e| e.to_string())
    }
}

/// Rename an indexed file and keep the SQLite/FTS metadata in sync.
/// Only a filename is accepted; directory traversal and cross-directory
/// moves are intentionally rejected.
#[tauri::command]
pub fn rename_path(
    state: State<'_, Arc<AppState>>,
    path: String,
    name: String,
) -> Result<String, String> {
    let old_path = crate::util::norm_path(&path);
    let source = Path::new(&old_path);
    if !source.is_file() {
        return Err("源文件不存在或不可访问".into());
    }
    let new_name = name.trim();
    if new_name.is_empty()
        || new_name == "."
        || new_name == ".."
        || new_name.chars().any(|c| c.is_control())
        || new_name.contains('/')
        || new_name.contains('\\')
    {
        return Err("新名称必须是有效的文件名".into());
    }
    // Windows rejects these names; checking here gives a consistent error on
    // every platform and prevents accidental device-path handling.
    if cfg!(windows) && ["CON", "PRN", "AUX", "NUL"].iter().any(|v| new_name.eq_ignore_ascii_case(v)) {
        return Err("新名称不能使用系统保留名称".into());
    }
    let row = state
        .db
        .get_by_path(&old_path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "文件不在当前索引中".to_string())?;
    let destination = source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(new_name);
    let new_path = crate::util::norm_path(&destination.to_string_lossy());
    if crate::util::norm_path(&row.path) != old_path {
        return Err("索引路径不一致，请重新扫描".into());
    }
    if crate::util::norm_path(&destination.to_string_lossy()) == old_path {
        return Ok(old_path);
    }
    if destination.exists() {
        return Err("目标文件名已存在".into());
    }
    std::fs::rename(source, &destination).map_err(|e| format!("重命名失败：{e}"))?;
    if let Err(error) = state.db.rename_file(&old_path, &new_path) {
        // Keep the filesystem and index consistent if the database write
        // fails. The rollback is best effort and never deletes user data.
        let _ = std::fs::rename(&destination, source);
        return Err(format!("更新索引失败：{error}"));
    }
    let _ = state.db.log("file", &format!("文件已重命名：{old_path} -> {new_path}"));
    Ok(new_path)
}

/// Lazily create a gallery thumbnail for document formats whose preview is
/// expensive. The file id is checked against the database before writing so
/// a stale UI result cannot make a thumbnail for an unrelated path.
#[tauri::command]
pub fn ensure_preview(
    state: State<'_, Arc<AppState>>,
    file_id: i64,
    path: String,
) -> Result<bool, String> {
    let row = state
        .db
        .get_by_id(file_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "文件不存在".to_string())?;
    if crate::util::norm_path(&row.path) != crate::util::norm_path(&path) {
        return Err("预览路径与索引记录不一致".into());
    }
    let source = Path::new(&row.path);
    if !source.is_file() {
        return Ok(false);
    }
    let thumb_dir = state.data_dir.join("thumbs");
    std::fs::create_dir_all(&thumb_dir).map_err(|error| error.to_string())?;
    let dest = thumb_dir.join(format!("{file_id}.jpg"));
    if !dest.is_file() || row.thumb_key.is_none() {
        let made = if matches!(row.kind.as_str(), "image" | "video") {
            crate::media::make_thumb(source, &dest, state.cfg.read().index.image_max_side)
        } else {
            crate::media::make_document_thumb(source, &dest, state.cfg.read().index.image_max_side)
        };
        if !made {
            return Ok(false);
        }
    }
    state
        .db
        .set_thumb(file_id, &format!("{file_id}.jpg"))
        .map_err(|error| error.to_string())?;
    Ok(true)
}

/// Persist a PDF.js-rendered first page so later searches can load a small
/// JPEG instead of reopening the full PDF in the WebView.
#[tauri::command]
pub fn save_pdf_thumbnail(
    state: State<'_, Arc<AppState>>,
    file_id: i64,
    path: String,
    data_url: String,
) -> Result<(), String> {
    use base64::Engine as _;

    let row = state.db.get_by_id(file_id).map_err(|e| e.to_string())?
        .ok_or_else(|| "文件不存在".to_string())?;
    if row.ext.trim_start_matches('.').eq_ignore_ascii_case("pdf") == false
        || crate::util::norm_path(&row.path) != crate::util::norm_path(&path)
    {
        return Err("PDF 缩略图来源与索引记录不匹配".into());
    }
    if !Path::new(&row.path).is_file() {
        return Err("PDF 文件已不存在".into());
    }
    let (mime, encoded) = data_url.split_once(',').ok_or_else(|| "PDF 缩略图数据无效".to_string())?;
    if mime != "data:image/jpeg;base64" {
        return Err("PDF 缩略图必须是 JPEG".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)
        .map_err(|e| format!("PDF 缩略图解码失败：{e}"))?;
    if bytes.len() < 4 || bytes.len() > 2 * 1024 * 1024 || bytes[..2] != [0xff, 0xd8] {
        return Err("PDF 缩略图大小或格式无效".into());
    }
    let thumb_dir = state.data_dir.join("thumbs");
    std::fs::create_dir_all(&thumb_dir).map_err(|e| e.to_string())?;
    let dest = thumb_dir.join(format!("{file_id}.jpg"));
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?.as_nanos();
    let temp = thumb_dir.join(format!("{file_id}.{nonce}.tmp"));
    std::fs::write(&temp, &bytes).map_err(|e| e.to_string())?;
    if let Err(error) = std::fs::rename(&temp, &dest) {
        if dest.is_file() {
            std::fs::remove_file(&dest).map_err(|remove_error| {
                let _ = std::fs::remove_file(&temp);
                format!("替换旧 PDF 缩略图失败：{remove_error}")
            })?;
            if let Err(replace_error) = std::fs::rename(&temp, &dest) {
                let _ = std::fs::remove_file(&temp);
                return Err(replace_error.to_string());
            }
        } else {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
    }
    state.db.set_thumb(file_id, &format!("{file_id}.jpg")).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn op_export(paths: Vec<String>, out: String, fmt: Option<String>) -> Result<usize, String> {
    fileops::export_listing(&paths, Path::new(&out), &fmt.unwrap_or_else(|| "csv".into()))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- 规则
#[tauri::command]
pub fn rules_list(state: State<'_, Arc<AppState>>) -> Vec<Rule> {
    state.db.list_rules().unwrap_or_default()
}

#[tauri::command]
pub fn rules_save(state: State<'_, Arc<AppState>>, rules: Vec<Rule>) -> Result<(), String> {
    state.db.replace_rules(&rules).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn rules_apply(state: State<'_, Arc<AppState>>) -> usize {
    fileops::apply_rules_to_db(&state.db)
}

// ---------------------------------------------------------------- 日志
#[tauri::command]
pub fn activity_list(state: State<'_, Arc<AppState>>, kind: Option<String>, limit: Option<usize>) -> Vec<Activity> {
    state.db.recent_activity(limit.unwrap_or(200), kind.as_deref()).unwrap_or_default()
}


// ---------------------------------------------------------------- 索引目录结构面板
//
// 右上角「143,522 个文件 · 54.5 GB」那个按钮点开就是这块数据：它不重新扫描，
// 只从 SQLite 的 dir 聚合里按层取，所以打开是毫秒级的。

/// 面板里的一个节点（目录或文件）。
#[derive(Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub is_file: bool,
    pub ext: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
    /// 目录的直属文件数；文件恒为 1。
    pub files: i64,
    /// 目录的直属子目录数。
    pub dirs: i64,
    /// 目录的递归文件总数（含所有子目录）。
    pub total_files: i64,
    pub total_bytes: i64,
    /// 已抽正文或已建向量的文件数（只对文件节点有意义）。
    pub indexed: i64,
    /// 排队中（pending / fail）的文件数。
    pub pending: i64,
    /// 索引根的错误信息；普通节点为空串。
    pub error: String,
}

/// 面板展开一层后返回的数据。
#[derive(Serialize)]
pub struct TreeLevel {
    pub path: String,
    pub name: String,
    pub entries: Vec<TreeEntry>,
    /// 当前目录的直属文件数 / 子目录数。
    pub files: i64,
    pub dirs: i64,
    pub total_files: i64,
    pub total_bytes: i64,
    pub indexed: i64,
    pub pending: i64,
    /// 直属文件超过上限、没有列出的数量。
    pub hidden_files: i64,
}

/// 按反斜杠切段并丢掉空段：`D:\` 得到 `["D:"]`。
fn split_path_parts(p: &str) -> Vec<&str> {
    p.split('\\').filter(|s| !s.is_empty()).collect()
}


fn path_display_name(path: &str) -> String {
    match split_path_parts(path).last() {
        // 盘符根（D:\）的最后一段是 "D:"，补回反斜杠，面板上显示成 "D:\"。
        Some(s) if s.len() == 2 && s.ends_with(':') => format!("{s}\\"),
        Some(s) => s.to_string(),
        None => path.to_string(),
    }
}

/// 索引根层：每个已添加的盘符 / 根目录一个节点。
fn tree_roots(db: &crate::db::Db) -> TreeLevel {
    let roots = db.index_roots().unwrap_or_default();
    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut total_files = 0i64;
    let mut total_bytes = 0i64;
    // 需求 #2（1TB 库卡顿）：原来对每个索引根各跑一次全子树 GROUP BY，把该根下
    // **每一层**目录都拉回内存再手工累加。现在全库目录聚合只跑一条 GROUP BY
    // （33,608 行 63 ms），每个根在内存里取自己那一份 —— 根再多也只扫一次库。
    let all_dirs = db.all_dir_rollup().unwrap_or_default();
    for r in roots {
        let root_key = dir_lookup_key(&r.root);
        let (folded, own) = fold_dir_children(&all_dirs, &root_key);
        let mut deep_files = own.0;
        let mut deep_bytes = own.1;
        for list in folded.get(&root_key).map(|v| v.as_slice()).unwrap_or(&[]) {
            deep_files += list.1;
            deep_bytes += list.2;
        }
        let files = own.0;
        let bytes = own.1;
        let dirs = folded.get(&root_key).map(|v| v.len()).unwrap_or(0) as i64;
        total_files += deep_files;
        total_bytes += deep_bytes;
        entries.push(TreeEntry {
            name: path_display_name(&r.root),
            path: r.root.clone(),
            is_file: false,
            ext: String::new(),
            kind: String::new(),
            size: bytes,
            mtime: r.last_scan,
            files,
            dirs,
            total_files: deep_files,
            total_bytes: deep_bytes,
            indexed: 0,
            pending: 0,
            error: r.last_error.clone(),
        });
    }
    TreeLevel {
        path: String::new(),
        name: String::new(),
        entries,
        files: 0,
        dirs: 0,
        total_files,
        total_bytes,
        indexed: 0,
        pending: 0,
        hidden_files: 0,
    }
}

/// 展开某个目录：直属子目录（按名字自然排序）+ 直属文件（按修改时间倒序，最多 200 条）。
fn tree_level(db: &crate::db::Db, path: &str, file_limit: usize) -> TreeLevel {
    let norm = crate::util::norm_path(path);
    // 需求 #2（1TB 库卡顿）：这里原来走 dir_rollup_under，会把该目录下
    // **每一层**的目录行全拉回内存再在 Rust 侧反推直接子目录聚合。28 万文件
    // 时是 33,608 行，1TB 库是数十万行，用户点开一层要付几十毫秒到秒级。
    // 现在只剩「全库目录聚合 + 内存折叠」一条固定开销的查询，外加一次
    // 走 ix_files_dir_size 的直属文件查询（实测 0.3 ms）。
    let (direct_files, _direct_bytes, dir_text_ok, dir_embed_ok) =
        db.dir_file_stats(&norm).unwrap_or((0, 0, 0, 0));

    let root_key = dir_lookup_key(&norm);
    let all_dirs = db.all_dir_rollup().unwrap_or_default();
    let (folded, own) = fold_dir_children(&all_dirs, &root_key);
    let children: Vec<(String, i64, i64)> = folded.get(&root_key).cloned().unwrap_or_default();
    // 递归总量：自己的直属 + 各直接子目录的子树合计。
    let mut total_files = own.0;
    let mut total_bytes = own.1;
    for (_, count, bytes) in &children {
        total_files += count;
        total_bytes += bytes;
    }
    // 各子目录的直属文件数：走一条 IN 查询（ix_files_dir_size 覆盖），
    // 子目录上百个也只有一次往返。折叠结果给的是子树合计，不是直属。
    let child_paths: Vec<String> = children.iter().map(|(p, _, _)| p.clone()).collect();
    let direct_counts = db.dir_direct_counts(&child_paths).unwrap_or_default();

    // 大小写不敏感去重：Windows 上 E:\A 与 e:\a 是同一个目录，但 files.dir
    // 保留的是写入时的原始大小写，GROUP BY 会把它分成两行。
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries: Vec<TreeEntry> = Vec::with_capacity(children.len() + 32);
    for (child_path, count, bytes) in children {
        if child_path.eq_ignore_ascii_case(&norm) {
            continue;
        }
        let name = path_display_name(&child_path);
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        let direct = direct_counts.get(&child_path).map(|v| v.0).unwrap_or(0);
        entries.push(TreeEntry {
            name,
            path: child_path,
            is_file: false,
            ext: String::new(),
            kind: String::new(),
            size: bytes,
            mtime: 0,
            files: direct,
            dirs: 0,
            total_files: count,
            total_bytes: bytes,
            indexed: 0,
            pending: 0,
            error: String::new(),
        });
    }
    // 与原 BTreeMap 一致：按名字升序，面板里的目录顺序不会跳动。
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let child_count = entries.len() as i64;

    // 直属文件：一次查询，按修改时间倒序，超出上限的部分用 hidden_files 提示。
    let file_rows = db.query_rows(
        "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
         FROM files WHERE missing=0 AND dir = ?1 ORDER BY mtime DESC LIMIT ?2",
        vec![
            rusqlite::types::Value::Text(norm.clone()),
            rusqlite::types::Value::Integer(file_limit as i64),
        ],
    );
    // 索引进度按「整个目录」算（含未列出的文件），否则只统计前 200 条会偏低。
    // dir_file_stats 已经在函数开头查过一次，这里直接复用，避免同一条聚合跑两遍。
    let indexed = dir_text_ok.max(dir_embed_ok);
    let pending = (direct_files - indexed).max(0);
    for f in &file_rows {
        let done = f.text_state == "ok" || f.embed_state == "ok";
        let waiting = f.text_state == "pending" || f.text_state == "fail";
        entries.push(TreeEntry {
            name: f.name.clone(),
            path: f.path.clone(),
            is_file: true,
            ext: f.ext.clone(),
            kind: f.kind.clone(),
            size: f.size,
            mtime: f.mtime,
            files: 1,
            dirs: 0,
            total_files: 1,
            total_bytes: f.size,
            indexed: if done { 1 } else { 0 },
            pending: if waiting { 1 } else { 0 },
            error: String::new(),
        });
    }

    TreeLevel {
        path: norm.clone(),
        name: path_display_name(&norm),
        entries,
        files: direct_files,
        dirs: child_count,
        total_files,
        total_bytes,
        indexed,
        pending,
        hidden_files: (direct_files - file_rows.len() as i64).max(0),
    }
}

/// 右上角面板的数据入口：不传 path 就是索引根层，传了就是那一层。
#[tauri::command]
pub fn tree_browse(
    state: State<'_, Arc<AppState>>,
    path: Option<String>,
    file_limit: Option<usize>,
) -> Result<TreeLevel, String> {
    let limit = file_limit.unwrap_or(200).clamp(1, 2000);
    match path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        Some(p) => Ok(tree_level(&state.db, &p, limit)),
        None => Ok(tree_roots(&state.db)),
    }
}

/// 面板内嵌搜索框：按目录名匹配，返回命中的目录（文件多的排前面）。
#[tauri::command]
pub fn tree_search(
    state: State<'_, Arc<AppState>>,
    query: String,
    limit: Option<usize>,
) -> Vec<TreeEntry> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let cap = limit.unwrap_or(200).clamp(1, 500);
    // 需求 #2：原来 `dir_rollup()` 会把全库每一个目录都拉进内存再逐行 contains，
    // 28 万文件库就是 3.3 万行 + 一次完整 GROUP BY；1TB 库会到数十万行。
    // 现在把名字过滤下推到 SQLite，只回真正命中的目录行。
    // 多取一倍：目录名相同、路径不同的几条要按文件数排序后截断。
    let rows = match state.db.dir_name_search(&q, cap.saturating_mul(4)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut hits: Vec<TreeEntry> = Vec::with_capacity(rows.len());
    for (dir, count, size) in rows {
        // LIKE 只是粗筛：它也会命中「父目录含该词」的路径（E:\pixiv2\x 之于
        // pixiv）。面板显示的是最后一段名字，所以这里再按名字精确过滤一次，
        // 语义与旧的 name.contains() 完全一致，只是候选集小了三个数量级。
        let name = path_display_name(&dir);
        if !name.to_lowercase().contains(&q) {
            continue;
        }
        hits.push(TreeEntry {
            name,
            path: dir,
            is_file: false,
            ext: String::new(),
            kind: String::new(),
            size,
            mtime: 0,
            files: count,
            dirs: 0,
            total_files: count,
            total_bytes: size,
            indexed: 0,
            pending: 0,
            error: String::new(),
        });
    }
    hits.sort_by(|a, b| b.total_files.cmp(&a.total_files).then(a.path.cmp(&b.path)));
    hits.truncate(cap);
    hits
}

/// 把某个目录加进「工作区实时检索」的目录列表（右键菜单用）。
#[tauri::command]
pub fn workspace_add(
    state: State<'_, Arc<AppState>>,
    dir: String,
) -> Result<Vec<String>, String> {
    let norm = crate::util::norm_path(&dir);
    if norm.trim().is_empty() {
        return Err("目录不能为空".into());
    }
    if !Path::new(&norm).is_dir() {
        return Err(format!("目录不存在或不可访问：{norm}"));
    }
    let mut cfg = state.cfg.read().clone();
    if !cfg.workspace.dirs.iter().any(|d| d.eq_ignore_ascii_case(&norm)) {
        cfg.workspace.dirs.push(norm.clone());
    }
    if !cfg.workspace.pinned_paths.iter().any(|d| d.eq_ignore_ascii_case(&norm)) {
        cfg.workspace.pinned_paths.push(norm.clone());
    }
    let path = crate::config::config_path(Some(&state.app_dir));
    cfg.save(&path).map_err(|e| format!("保存配置失败：{e}"))?;
    let dirs = cfg.workspace.dirs.clone();
    *state.cfg.write() = cfg;
    let _ = state.db.log("workspace", &format!("加入工作区：{norm}"));
    Ok(dirs)
}

#[derive(Serialize)]
pub struct WorkspaceStateOut {
    pub recent_paths: Vec<String>,
    pub pinned_paths: Vec<String>,
    pub last_path: String,
    pub last_scope: String,
    pub startup_scope: String,
    pub startup_path: String,
    pub initial_scope: String,
    pub initial_path: String,
}

fn workspace_state_from(cfg: &AppConfig) -> WorkspaceStateOut {
    let (initial_scope, initial_path) = match cfg.workspace.startup_scope.as_str() {
        "all" => ("all".to_string(), String::new()),
        "indexed" => ("indexed".to_string(), String::new()),
        "fixed" if Path::new(&cfg.workspace.startup_path).is_dir() => {
            ("path".to_string(), cfg.workspace.startup_path.clone())
        }
        "remember" if cfg.workspace.last_scope == "path"
            && Path::new(&cfg.workspace.last_path).is_dir() =>
        {
            ("path".to_string(), cfg.workspace.last_path.clone())
        }
        "remember" if cfg.workspace.last_scope == "all" => ("all".to_string(), String::new()),
        _ => ("indexed".to_string(), String::new()),
    };
    WorkspaceStateOut {
        recent_paths: cfg.workspace.recent_paths.clone(),
        pinned_paths: cfg.workspace.pinned_paths.clone(),
        last_path: cfg.workspace.last_path.clone(),
        last_scope: cfg.workspace.last_scope.clone(),
        startup_scope: cfg.workspace.startup_scope.clone(),
        startup_path: cfg.workspace.startup_path.clone(),
        initial_scope,
        initial_path,
    }
}

/// Return persisted home-page workspace choices. This is intentionally
/// separate from `get_config` so the search view can refresh it cheaply.
#[tauri::command]
pub fn workspace_state(state: State<'_, Arc<AppState>>) -> WorkspaceStateOut {
    workspace_state_from(&state.cfg.read())
}

#[tauri::command]
pub fn workspace_remember(
    state: State<'_, Arc<AppState>>,
    path: String,
    scope: Option<String>,
) -> Result<WorkspaceStateOut, String> {
    let norm = crate::util::norm_path(&path);
    if norm.trim().is_empty() || !Path::new(&norm).is_dir() {
        return Err("工作区路径不存在或不是目录".into());
    }
    let mut cfg = state.cfg.read().clone();
    cfg.workspace.recent_paths.retain(|p| !p.eq_ignore_ascii_case(&norm));
    cfg.workspace.recent_paths.insert(0, norm.clone());
    cfg.workspace.recent_paths.truncate(3);
    cfg.workspace.last_path = norm;
    if let Some(scope) = scope {
        let scope = scope.trim().to_ascii_lowercase();
        if matches!(scope.as_str(), "all" | "indexed" | "path") {
            cfg.workspace.last_scope = scope;
        }
    } else {
        cfg.workspace.last_scope = "path".into();
    }
    let path = crate::config::config_path(Some(&state.app_dir));
    cfg.save(&path).map_err(|e| format!("保存工作区记忆失败：{e}"))?;
    let out = workspace_state_from(&cfg);
    *state.cfg.write() = cfg;
    Ok(out)
}

#[tauri::command]
pub fn workspace_set_active(
    state: State<'_, Arc<AppState>>,
    scope: String,
    path: Option<String>,
) -> Result<WorkspaceStateOut, String> {
    let scope = scope.trim().to_ascii_lowercase();
    if !matches!(scope.as_str(), "all" | "indexed" | "path") {
        return Err("无效的工作区搜索范围".into());
    }
    let mut cfg = state.cfg.read().clone();
    if scope == "path" {
        let norm = crate::util::norm_path(path.as_deref().unwrap_or_default());
        if norm.is_empty() || !Path::new(&norm).is_dir() {
            return Err("工作区路径不存在或不是目录".into());
        }
        cfg.workspace.recent_paths.retain(|p| !p.eq_ignore_ascii_case(&norm));
        cfg.workspace.recent_paths.insert(0, norm.clone());
        cfg.workspace.recent_paths.truncate(3);
        cfg.workspace.last_path = norm;
    }
    cfg.workspace.last_scope = scope;
    let config_path = crate::config::config_path(Some(&state.app_dir));
    cfg.save(&config_path).map_err(|e| format!("保存工作区记忆失败：{e}"))?;
    let out = workspace_state_from(&cfg);
    *state.cfg.write() = cfg;
    Ok(out)
}

#[tauri::command]
pub fn workspace_remove(
    state: State<'_, Arc<AppState>>,
    path: String,
) -> Result<WorkspaceStateOut, String> {
    let norm = crate::util::norm_path(&path);
    let mut cfg = state.cfg.read().clone();
    cfg.workspace.dirs.retain(|p| !p.eq_ignore_ascii_case(&norm));
    cfg.workspace.pinned_paths.retain(|p| !p.eq_ignore_ascii_case(&norm));
    cfg.workspace.recent_paths.retain(|p| !p.eq_ignore_ascii_case(&norm));
    if cfg.workspace.startup_path.eq_ignore_ascii_case(&norm) {
        cfg.workspace.startup_path.clear();
        if cfg.workspace.startup_scope == "fixed" {
            cfg.workspace.startup_scope = "indexed".into();
        }
    }
    if cfg.workspace.last_path.eq_ignore_ascii_case(&norm) {
        cfg.workspace.last_path.clear();
        cfg.workspace.last_scope = "indexed".into();
    }
    let config_path = crate::config::config_path(Some(&state.app_dir));
    cfg.save(&config_path).map_err(|e| format!("保存工作区列表失败：{e}"))?;
    let out = workspace_state_from(&cfg);
    *state.cfg.write() = cfg;
    Ok(out)
}

#[tauri::command]
pub fn workspace_set_startup(
    state: State<'_, Arc<AppState>>,
    scope: String,
    path: Option<String>,
) -> Result<WorkspaceStateOut, String> {
    let scope = scope.trim().to_ascii_lowercase();
    if !matches!(scope.as_str(), "remember" | "all" | "indexed" | "fixed") {
        return Err("无效的启动工作区范围".into());
    }
    let fixed = path.unwrap_or_default();
    let fixed = if fixed.trim().is_empty() { String::new() } else { crate::util::norm_path(&fixed) };
    if scope == "fixed" && (fixed.is_empty() || !Path::new(&fixed).is_dir()) {
        return Err("固定工作区路径不存在或不是目录".into());
    }
    let mut cfg = state.cfg.read().clone();
    cfg.workspace.startup_scope = scope;
    cfg.workspace.startup_path = fixed;
    if cfg.workspace.startup_scope == "fixed"
        && !cfg.workspace.pinned_paths.iter().any(|p| p.eq_ignore_ascii_case(&cfg.workspace.startup_path))
    {
        cfg.workspace.pinned_paths.push(cfg.workspace.startup_path.clone());
    }
    let config_path = crate::config::config_path(Some(&state.app_dir));
    cfg.save(&config_path).map_err(|e| format!("保存启动工作区失败：{e}"))?;
    let out = workspace_state_from(&cfg);
    *state.cfg.write() = cfg;
    Ok(out)
}

/// List mounted Windows drive roots for the home-page scope selector.
#[tauri::command]
pub fn available_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|letter| format!("{letter}:\\"))
            .filter(|root| Path::new(root).is_dir())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".into()]
    }
}

// ---------------------------------------------------------------- 星系图谱
//
// 需求（REQ-042~052 / REQ-066~069）：图谱视图与搜索结果列表共享 GraphNode /
// GraphEdge 数据模型。这里只做「一次取一层」的骨架数据，展开子层由前端用
// tree_browse 增量追加，避免 14 万文件时把整棵树塞进一次 IPC。
#[derive(Serialize)]
pub struct GraphNode {
    /// 稳定 ID：文件夹用完整路径，文件用 `f:<路径>`，前端据此做去重与增量合并。
    pub id: String,
    /// "folder" | "file"
    pub node_type: String,
    pub name: String,
    pub path: String,
    /// 文件类型：image/video/audio/document/text/archive/app/other；文件夹为空串。
    pub kind: String,
    pub size: i64,
    /// 文件夹的递归文件数；文件恒为 1。
    pub count: i64,
    /// 相对根的层级，根为 0。
    pub depth: i64,
    pub mtime: i64,
}

#[derive(Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    /// contains | similar
    pub edge_type: String,
    pub strength: f64,
}

#[derive(Serialize)]
pub struct GraphData {
    pub root: String,
    pub name: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub total_files: i64,
    pub total_bytes: i64,
    /// 因 max_nodes 被折叠掉的目录数（前端显示「+N」）。
    pub truncated: i64,
}

/// 图谱骨架：根 + `depth` 层目录 + 每层目录下「按大小取前 N」的文件。
#[tauri::command]
pub fn graph_data(
    state: State<'_, Arc<AppState>>,
    root: Option<String>,
    depth: Option<usize>,
    max_nodes: Option<usize>,
    file_limit: Option<usize>,
) -> GraphData {
    let depth = depth.unwrap_or(2).clamp(1, 6);
    let max_nodes = max_nodes.unwrap_or(900).clamp(40, 8000);
    let file_limit = file_limit.unwrap_or(31).clamp(0, 500);

    // 根：显式传入优先，否则取第一个索引根；再不行就用全库。
    let root = root
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            state
                .db
                .index_roots()
                .ok()
                .and_then(|v| v.into_iter().map(|r| r.root).find(|r| !r.trim().is_empty()))
                .unwrap_or_default()
        });
    let root = if root.is_empty() { String::new() } else { crate::util::norm_path(&root) };

    // 需求 #2（1TB 库卡顿）：这里原来调 dir_rollup_under(&root) 把**整棵子树**
    // 每一层目录一次性拉回内存（1TB 库里可达数十万行），再建 HashMap 反推父聚合。
    // 现在整棵树只在内存里折叠一次（all_dir_rollup 一条 GROUP BY，33,608 行
    // 实测 63 ms），root 的递归合计也从这个结果里顺手累加，不再单独跑
    // dir_recursive_stats（28 万行库上那一条要 142 ms）。
    // 逐层向下建目录节点，每层最多保留 max_nodes 个（按递归文件数降序）。
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut truncated = 0i64;

    let mut root_key = split_path_parts(&root).join("\\");
    // 全库目录聚合一次拿回来（63 ms / 33,608 行），在内存里折叠成
    // 「目录 → 直接子目录 + 子树合计」，同时把 root 自己的直属统计带出来。
    // 逐层展开时只想拿当前层，但每层重新查 SQL 的代价是 290 ms × 目录数，
    // 那才是首屏 17 秒的元凶。图里 root 的递归总量也由它给出，
    // 不再单独跑 dir_recursive_stats。
    let (dir_children, root_own) = {
        let rows = state.db.all_dir_rollup().unwrap_or_default();
        fold_dir_children(&rows, &root_key)
    };
    let mut total_files = root_own.0;
    let mut total_bytes = root_own.1;
    for list in dir_children.get(&root_key).map(|v| v.as_slice()).unwrap_or(&[]) {
        total_files += list.1;
        total_bytes += list.2;
    }
    // 用库里的真实大小写：第 0 层的直属文件查询是 `dir = ?1`（BINARY 比较），
    // 外部传进来的 `d:\foo` 与库里的 `D:\Foo` 对不上就会一片空白。
    // 只有在真的查不到时才付一次 NOCASE 扫描的代价，正常路径零成本。
    if total_files == 0 && !root_key.is_empty() {
        if let Some(canon) = state.db.canonical_dir(&root_key) {
            root_key = canon;
        }
    }
    let root_count = total_files;
    let root_bytes = total_bytes;
    let root_id = if root.is_empty() { ":root".to_string() } else { root_key.clone() };
    nodes.push(GraphNode {
        id: root_id.clone(),
        node_type: "folder".into(),
        name: path_display_name(&root_key),
        path: root_key.clone(),
        kind: String::new(),
        size: root_bytes,
        count: root_count,
        depth: 0,
        mtime: 0,
    });

    // level_dirs：当前层的目录 (路径, 深度)。逐层展开，超出预算的折叠计数。
    let mut level_dirs: Vec<(String, i64)> = vec![(root_key.clone(), 0)];
    for level in 0..depth {
        let mut next: Vec<(String, i64)> = Vec::new();
        for dir in &level_dirs {
            // 该目录的直属子目录：从内存折叠结果里取，不再进 SQL。
            // dir_children 是「目录 → 直接子目录聚合」的整棵树（一次构建，
            // 逐层复用），这里只做预算裁剪。
            let dir_norm = crate::util::norm_path(&dir.0);
            let mut children: Vec<(String, i64, i64)> =
                dir_children.get(&dir_norm).cloned().unwrap_or_default();
            if children.len() > max_nodes.saturating_sub(nodes.len()) {
                let keep = max_nodes.saturating_sub(nodes.len());
                truncated += (children.len() - keep) as i64;
                children.truncate(keep);
            }
            for (child_path, count, bytes) in children {
                let id = child_path.clone();
                nodes.push(GraphNode {
                    id: id.clone(),
                    node_type: "folder".into(),
                    name: path_display_name(&child_path),
                    path: child_path.clone(),
                    kind: String::new(),
                    size: bytes,
                    count,
                    depth: level as i64 + 1,
                    mtime: 0,
                });
                edges.push(GraphEdge {
                    source: if dir_norm.is_empty() { ":root".into() } else { dir_norm.clone() },
                    target: id,
                    edge_type: "contains".into(),
                    strength: 1.0,
                });
                if level + 1 < depth {
                    next.push((child_path, level as i64 + 1));
                }
            }

            // 直属文件：按大小降序取前 file_limit 个（需求 REQ-035 抽样展示）。
            if file_limit > 0 {
                let files = state.db.query_rows(
                    "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
                     FROM files WHERE missing=0 AND dir = ?1 ORDER BY size DESC, mtime DESC LIMIT ?2",
                    vec![
                        rusqlite::types::Value::Text(dir_norm.clone()),
                        rusqlite::types::Value::Integer(file_limit as i64),
                    ],
                );
                let mut wanted: Vec<&FileRow> = files.iter().collect();
                let room = max_nodes.saturating_sub(nodes.len());
                if wanted.len() > room {
                    truncated += (wanted.len() - room) as i64;
                    wanted.truncate(room);
                }
                for f in wanted {
                    let id = format!("f:{}", f.path);
                    nodes.push(GraphNode {
                        id: id.clone(),
                        node_type: "file".into(),
                        name: f.name.clone(),
                        path: f.path.clone(),
                        kind: f.kind.clone(),
                        size: f.size,
                        count: 1,
                        depth: level as i64 + 1,
                        mtime: f.mtime,
                    });
                    edges.push(GraphEdge {
                        source: if dir_norm.is_empty() { ":root".into() } else { dir_norm.clone() },
                        target: id,
                        edge_type: "contains".into(),
                        strength: 1.0,
                    });
                }
            }
            if nodes.len() >= max_nodes {
                break;
            }
        }
        if next.is_empty() || nodes.len() >= max_nodes {
            break;
        }
        level_dirs = next;
    }

    GraphData {
        name: path_display_name(&root_key),
        // 回传库里的真实大小写与规范分隔符，前端面包屑与「以此为根」才能对上。
        root: root_key,
        nodes,
        edges,
        total_files,
        total_bytes,
        truncated,
    }
}


// ---------------------------------------------------------------- 思维模型图
//
// 需求 REQ-053~061：八种思维模型图（界定 / 描述 / 比较 / 分类 / 拆解 / 顺序 /
// 因果 / 类比）。八种图共用同一个「分组 + 条目」的数据形状，前端只负责画法差异，
// 这样后端一次查询就能支撑全部模型，切换模型不需要重新打磁盘。
#[derive(Serialize)]
pub struct MindItem {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub ext: String,
    pub size: i64,
    pub mtime: i64,
    pub dir: String,
}

#[derive(Serialize)]
pub struct MindGroup {
    pub key: String,
    pub label: String,
    pub count: i64,
    pub bytes: i64,
    pub color: String,
    pub items: Vec<MindItem>,
}

#[derive(Serialize)]
pub struct MindMapOut {
    pub model: String,
    pub title: String,
    pub subtitle: String,
    pub groups: Vec<MindGroup>,
    pub total_files: i64,
    pub total_bytes: i64,
    pub buckets: Vec<String>,
}

/// 每个分组最多带回多少条目（前端做「显示更多」）。
const MIND_GROUP_CAP: usize = 120;

fn kind_label_of(kind: &str) -> &'static str {
    match kind {
        "image" => "图片",
        "video" => "视频",
        "audio" => "音频",
        "document" => "文档",
        "text" => "文本",
        "archive" => "压缩包",
        "app" => "程序",
        _ => "其他",
    }
}

fn kind_color_of(kind: &str) -> &'static str {
    match kind {
        "image" => "#B07AA1",
        "video" => "#E15759",
        "audio" => "#76B7B2",
        "document" => "#59A14F",
        "text" => "#F28E2B",
        "archive" => "#EDC948",
        "app" => "#FF9DA7",
        _ => "#9C755F",
    }
}

/// 时间桶标签：2026-10 / 2026-09 …（顺序图、作用图共用）。
fn month_bucket(mtime: i64) -> String {
    if mtime <= 0 {
        return "未知时间".into();
    }
    let days = mtime / 86_400;
    // 从 1970-01-01 起算的民用历法换算（不需要时区信息，够用即可）
    let mut year = 1970i64;
    let mut rest = days;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if rest < len {
            break;
        }
        rest -= len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let lens = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    for l in lens {
        if rest < l {
            break;
        }
        rest -= l;
        month += 1;
    }
    format!("{}-{:02}", year, month)
}

fn mind_item(row: &FileRow) -> MindItem {
    MindItem {
        name: row.name.clone(),
        path: row.path.clone(),
        kind: row.kind.clone(),
        ext: row.ext.clone(),
        size: row.size,
        mtime: row.mtime,
        dir: row.dir.clone(),
    }
}

/// 按 key 归组的小工具：保持首次出现顺序，超出上限的只计数不返回条目。
fn push_group(
    groups: &mut Vec<MindGroup>,
    key: &str,
    label: &str,
    color: &str,
    item: MindItem,
) {
    if let Some(g) = groups.iter_mut().find(|g| g.key == key) {
        g.count += 1;
        g.bytes += item.size;
        if g.items.len() < MIND_GROUP_CAP {
            g.items.push(item);
        }
        return;
    }
    let mut g = MindGroup {
        key: key.to_string(),
        label: label.to_string(),
        count: 1,
        bytes: item.size,
        color: color.to_string(),
        items: Vec::new(),
    };
    g.items.push(item);
    groups.push(g);
}

/// 八种思维模型图的数据。paths 为空时退回「最近 400 个已索引文件」。
#[tauri::command]
pub fn mind_map_data(
    state: State<'_, Arc<AppState>>,
    model: String,
    paths: Vec<String>,
    query: Option<String>,
) -> MindMapOut {
    // ---- 取数据：按路径 ID 分批查，单批 400 个避免 SQLite 变量上限。
    let mut rows: Vec<FileRow> = Vec::new();
    let wanted: Vec<String> = paths
        .iter()
        .map(|p| crate::util::norm_path(p))
        .filter(|p| !p.trim().is_empty())
        .take(2000)
        .collect();
    if wanted.is_empty() {
        rows = state.db.query_rows(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
             FROM files WHERE missing=0 ORDER BY mtime DESC LIMIT 400",
            vec![],
        );
    } else {
        for chunk in wanted.chunks(400) {
            let mut sql = String::from(
                "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
                 FROM files WHERE path IN (",
            );
            let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len());
            for (i, p) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&format!("?{}", i + 1));
                args.push(rusqlite::types::Value::Text(p.clone()));
            }
            sql.push(')');
            rows.extend(state.db.query_rows(&sql, args));
        }
    }

    let mut total_files = 0i64;
    let mut total_bytes = 0i64;
    let mut groups: Vec<MindGroup> = Vec::new();
    let query = query.unwrap_or_default();
    let query = query.trim().to_string();

    match model.as_str() {
        // REQ-053 界定图：回答「这是什么」，按文件类型分组。
        "definition" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                push_group(&mut groups, k, kind_label_of(k), kind_color_of(k), mind_item(r));
            }
        }
        // REQ-054 描述图：中心辐射 —— 以目录为辐射源。
        "description" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let label = path_display_name(&r.dir);
                push_group(&mut groups, &r.dir, &label, kind_color_of(&r.kind), mind_item(r));
            }
        }
        // REQ-055 比较图：两两比较 —— 按类型分集合，前端画韦恩图。
        "comparison" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                push_group(&mut groups, k, kind_label_of(k), kind_color_of(k), mind_item(r));
            }
            groups.sort_by(|a, b| b.count.cmp(&a.count));
        }
        // REQ-056 分类图：树状分类 —— 按扩展名分层。
        "classification" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let ext = if r.ext.is_empty() { "(无扩展名)".to_string() } else { format!(".{}", r.ext) };
                push_group(&mut groups, &ext, &ext, kind_color_of(&r.kind), mind_item(r));
            }
            groups.sort_by(|a, b| b.count.cmp(&a.count));
        }
        // REQ-057 拆解图：从整体拆到部分组成 —— 类型 → 扩展名两级。
        "decomposition" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                let ext = if r.ext.is_empty() { "无扩展名".to_string() } else { r.ext.clone() };
                let key = format!("{}/{}", k, ext);
                let label = format!("{} · {}", kind_label_of(k), ext);
                push_group(&mut groups, &key, &label, kind_color_of(k), mind_item(r));
            }
            groups.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        }
        // REQ-058 顺序图：时间线 —— 按月分桶。
        "sequence" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let bucket = month_bucket(r.mtime);
                push_group(&mut groups, &bucket, &bucket, kind_color_of(&r.kind), mind_item(r));
            }
            groups.sort_by(|a, b| b.key.cmp(&a.key));
        }
        // REQ-059 因果图：桑基 —— 「目录 → 类型」的流转关系。
        "causality" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                let label = path_display_name(&r.dir);
                push_group(&mut groups, &format!("{}|{}", r.dir, k), &format!("{} → {}", label, kind_label_of(k)), kind_color_of(k), mind_item(r));
            }
            groups.sort_by(|a, b| b.count.cmp(&a.count));
        }
        // REQ-060 类比图：平行对照 —— 按「同类同扩展名」配对。
        "analogy" => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                let ext = if r.ext.is_empty() { "- " } else { r.ext.as_str() };
                push_group(&mut groups, &format!("{}:{}", k, ext), &format!("{} · {}", kind_label_of(k), ext), kind_color_of(k), mind_item(r));
            }
            groups.sort_by(|a, b| b.count.cmp(&a.count));
        }
        // REQ-061 默认：按文件类型（与界定图一致，保证未知模型也有输出）。
        _ => {
            for r in &rows {
                total_files += 1;
                total_bytes += r.size;
                let k = if r.kind.is_empty() { "other" } else { r.kind.as_str() };
                push_group(&mut groups, k, kind_label_of(k), kind_color_of(k), mind_item(r));
            }
        }
    }

    let (title, subtitle) = match model.as_str() {
        "definition" => ("界定图".to_string(), "这批文件「是什么」——按类型界定边界".to_string()),
        "description" => ("描述图".to_string(), "这批文件「有什么特征」——从目录向外辐射".to_string()),
        "comparison" => ("比较图".to_string(), "这批文件「有什么不同」——各类型的规模对比".to_string()),
        "classification" => ("分类图".to_string(), "这批文件「怎么归类」——按扩展名分层".to_string()),
        "decomposition" => ("拆解图".to_string(), "这批文件「由什么构成」——类型拆到扩展名".to_string()),
        "sequence" => ("顺序图".to_string(), "这批文件「按时间怎么排」——月份时间线".to_string()),
        "causality" => ("因果图".to_string(), "这批文件「从哪来到哪去」——目录到类型的流向".to_string()),
        "analogy" => ("类比图".to_string(), "这批文件「像什么」——同类同格式的平行对照".to_string()),
        _ => ("思维模型图".to_string(), "按文件类型分组".to_string()),
    };

    let buckets: Vec<String> = if model == "sequence" {
        groups.iter().map(|g| g.key.clone()).collect()
    } else {
        Vec::new()
    };

    MindMapOut {
        model,
        title: if query.is_empty() { title } else { format!("{} · {}", title, query) },
        subtitle,
        groups,
        total_files,
        total_bytes,
        buckets,
    }
}

// ---------------------------------------------------------------- 树状图
#[derive(Serialize)]
pub struct TreeNode {
    pub path: String,
    pub name: String,
    pub size: i64,
    pub count: i64,
    pub kinds: std::collections::HashMap<String, i64>,
    pub children: Vec<TreeNode>,
}

#[tauri::command]
pub fn tree_data(state: State<'_, Arc<AppState>>, root: Option<String>, depth: Option<usize>) -> TreeNode {
    let depth = depth.unwrap_or(3).clamp(1, 8);
    // 需求 #2（1TB 库卡顿）：原来把 files 整表拉进内存（现在 28.1 万行，1 TB 库是
    // 几百万行）再逐行 reduce。改成先在库里按 (dir, kind) 聚合：28 万行库上只回
    // 37,264 行 / 约 310 ms，且规模只跟「目录 × 类型」数走，与文件总数脱钩。
    let rows = state.db.all_dir_kind_rollup().unwrap_or_default();
    // 不指定根目录时，从「所有已索引文件的公共父目录」开始，而不是从盘符（C:\）
    // 开始 —— 否则用户要连点五六层才能看到自己的文件夹。
    let root = match root {
        Some(r) if !r.trim().is_empty() => r,
        _ => smart_root(&rows).unwrap_or_default(),
    };
    build_tree(&rows, &root, depth)
}

/// 树状图单个节点的子树聚合（文件数、字节数、各类文件数）。
#[derive(Default, Clone)]
struct TreeSlice {
    count: i64,
    size: i64,
    kinds: std::collections::HashMap<String, i64>,
}

/// 全库「文件数最多的那条分支」，给树状图当默认根。
///
/// 先取所有目录的最长公共前缀：段数 > 1 就直接用它；只剩盘符时说明数据横跨
/// 多个一级目录，此时退化成「文件数最多的一级目录」，避免用户从盘符开始连点。
fn smart_root(rows: &[(String, String, i64, i64)]) -> Option<String> {
    let mut per_dir: HashMap<String, (i64, i64)> = HashMap::new();
    for (dir, _kind, count, bytes) in rows {
        if dir.is_empty() {
            continue;
        }
        let e = per_dir.entry(dir.clone()).or_insert((0, 0));
        e.0 += count;
        e.1 += bytes;
    }
    if per_dir.is_empty() {
        return None;
    }
    // 公共父目录：逐条求前缀交集（大小写不敏感，保留第一次见到的写法）。
    let mut common: Option<Vec<String>> = None;
    for dir in per_dir.keys() {
        let parts: Vec<String> = dir
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        common = Some(match common {
            None => parts,
            Some(prev) => {
                let mut n = 0;
                while n < prev.len() && n < parts.len() && prev[n].eq_ignore_ascii_case(&parts[n]) {
                    n += 1;
                }
                prev[..n].to_vec()
            }
        });
    }
    let common = common.unwrap_or_default();
    if common.len() > 1 {
        return Some(common.join("\\"));
    }
    // 退化成盘符级：按「盘符 + 一级目录」计票，取文件数最多的那条。
    let mut tally: HashMap<String, (String, i64, i64)> = HashMap::new();
    for (dir, (count, bytes)) in &per_dir {
        let ps: Vec<&str> = dir.split('\\').filter(|s| !s.is_empty()).collect();
        if ps.is_empty() {
            continue;
        }
        let (key, shown) = if ps.len() >= 2 {
            (
                format!("{}\\{}", ps[0].to_lowercase(), ps[1].to_lowercase()),
                format!("{}\\{}", ps[0], ps[1]),
            )
        } else {
            (ps[0].to_lowercase(), ps[0].to_string())
        };
        let e = tally.entry(key).or_insert((shown, 0, 0));
        e.1 += count;
        e.2 += bytes;
    }
    let best = tally
        .into_iter()
        .max_by_key(|(_, (_, n, b))| (*n, *b))
        .map(|(_, (shown, _, _))| shown);
    Some(best.unwrap_or_else(|| common.join("\\")))
}

/// 用 (dir, kind, count, bytes) 聚合行建树。
///
/// 一条 dir 行代表「某个目录里某一类文件的合计」。对每条行沿祖先链逐级累加，
/// 就能得到每一层目录的子树合计 —— 每个节点只被遍历一次，不做重复扫描。
/// 目录 key 统一小写（Windows 路径大小写不敏感），显示名保留原始写法。
fn build_tree(rows: &[(String, String, i64, i64)], root: &str, depth: usize) -> TreeNode {
    let root_parts: Vec<String> = split_path_parts(root)
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect();
    let root_len = root_parts.len();
    // 能累加到的最深层：root 是第 1 层，所以第 depth 层是 root_len + depth - 1。
    let max_level = root_len + depth.saturating_sub(1);
    let mut agg: HashMap<String, TreeSlice> = HashMap::new();
    // 小写 key -> 原始大小写路径（第一次出现为准，用于显示与下钻）。
    let mut display: HashMap<String, String> = HashMap::new();
    // 父节点 key -> 直接子节点 key（BTreeSet 保证字典序，与原 keys.sort() 一致）。
    let mut children: HashMap<String, std::collections::BTreeSet<String>> = HashMap::new();
    let mut total = TreeSlice::default();

    for (dir, kind, count, bytes) in rows {
        let parts: Vec<&str> = dir.split('\\').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let low: Vec<String> = parts.iter().map(|s| s.to_lowercase()).collect();
        // 只统计 root 子树内的目录（按段比较，D:\wx 不会误吞 D:\wxy）。
        if low.len() < root_len || low[..root_len] != root_parts[..] {
            continue;
        }
        total.count += count;
        total.size += bytes;
        *total.kinds.entry(kind.clone()).or_insert(0) += count;

        let limit = low.len().min(max_level);
        let mut key = String::new();
        let mut prev: Option<String> = None;
        for k in 0..limit {
            if k > 0 {
                key.push('\\');
            }
            key.push_str(&low[k]);
            let slot = agg.entry(key.clone()).or_default();
            slot.count += count;
            slot.size += bytes;
            *slot.kinds.entry(kind.clone()).or_insert(0) += count;
            display
                .entry(key.clone())
                .or_insert_with(|| parts[..=k].join("\\"));
            // 从 root 的下一层开始登记父子边（parent 是 root 时 k == root_len）。
            if let Some(parent) = &prev {
                if k >= root_len {
                    children
                        .entry(parent.clone())
                        .or_default()
                        .insert(key.clone());
                }
            } else if root_len == 0 {
                // root 为空 = 「全部」视图，第一段（盘符）直接挂在根节点下。
                children
                    .entry(String::new())
                    .or_default()
                    .insert(key.clone());
            }
            prev = Some(key.clone());
        }
    }

    let root_name = if root.is_empty() {
        "全部".to_string()
    } else {
        Path::new(root)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| root.to_string())
    };
    let root_key: String = root_parts.join("\\");
    let mut node = TreeNode {
        path: root.to_string(),
        name: root_name,
        size: total.size,
        count: total.count,
        kinds: total.kinds,
        children: Vec::new(),
    };
    node.children = tree_children(&root_key, depth, &agg, &display, &children);
    node
}

fn tree_children(
    parent: &str,
    depth_left: usize,
    agg: &HashMap<String, TreeSlice>,
    display: &HashMap<String, String>,
    children: &HashMap<String, std::collections::BTreeSet<String>>,
) -> Vec<TreeNode> {
    if depth_left <= 1 {
        return Vec::new();
    }
    let Some(keys) = children.get(parent) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(slice) = agg.get(key) else {
            continue;
        };
        let path = display.get(key).cloned().unwrap_or_else(|| key.clone());
        let name = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        out.push(TreeNode {
            path,
            name,
            size: slice.size,
            count: slice.count,
            kinds: slice.kinds.clone(),
            children: tree_children(key, depth_left - 1, agg, display, children),
        });
    }
    out
}

// ---------------------------------------------------------------- 监控
#[derive(Serialize)]
pub struct WatchStatus {
    pub running: bool,
    pub roots: Vec<String>,
}

#[tauri::command]
pub fn watch_start(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    let cfg = state.cfg.read().clone();
    let roots = if cfg.watch.roots.is_empty() {
        cfg.scan.roots.clone()
    } else {
        cfg.watch.roots.clone()
    };
    if roots.is_empty() {
        return Err("请先设置监控目录".into());
    }
    let n = state.watcher.start(&roots, cfg.watch.debounce_secs as f64, Arc::clone(&state.db))?;
    let _ = state.db.log("watch", &format!("开始监控 {} 个目录", n));
    Ok(n)
}

#[tauri::command]
pub fn watch_stop(state: State<'_, Arc<AppState>>) {
    state.watcher.stop();
    let _ = state.db.log("watch", "停止监控");
}

#[tauri::command]
pub fn watch_status(state: State<'_, Arc<AppState>>) -> WatchStatus {
    let cfg = state.cfg.read().clone();
    WatchStatus {
        running: state.watcher.running(),
        roots: if cfg.watch.roots.is_empty() { cfg.scan.roots } else { cfg.watch.roots },
    }
}

// ---------------------------------------------------------------- 局域网
#[derive(Serialize)]
pub struct ServerStatus {
    pub running: bool,
    pub url: Option<String>,
    pub port: u16,
}

#[tauri::command]
pub fn server_start(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let cfg = state.cfg.read().clone();
    let p = state.search_engine();
    let st = crate::server::ServerState {
        db: p.db.clone(),
        vectors: p.vectors.clone(),
        media_vectors: p.media.clone(),
        clip_vectors: p.clip.clone(),
        audio_vectors: p.audio.clone(),
        cfg: Arc::new(p.cfg.clone()),
        scan_roots: cfg.scan.roots.clone(),
        disabled_roots: cfg.scan.disabled_roots.clone(),
        token: cfg.server.token.clone(),
        allow_write: cfg.server.allow_write,
        data_dir: state.data_dir.clone(),
    };
    let url = state.server.start(&cfg.server, st)?;
    let _ = state.db.log("server", &format!("局域网服务启动 {url}"));
    Ok(url)
}

#[tauri::command]
pub fn server_stop(state: State<'_, Arc<AppState>>) {
    state.server.stop();
    let _ = state.db.log("server", "局域网服务停止");
}

#[tauri::command]
pub fn server_status(state: State<'_, Arc<AppState>>) -> ServerStatus {
    let cfg = state.cfg.read().clone();
    ServerStatus { running: state.server.running(), url: state.server.url(), port: cfg.server.port }
}

// ---------------------------------------------------------------- 杂项
#[tauri::command]
pub fn db_vacuum(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.db.vacuum().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn db_clear(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    // Stop and join the worker before touching SQLite/vector files. Otherwise
    // an in-flight encode could finish after the clear and repopulate part of
    // the index the user just confirmed clearing.
    if let Some(indexer) = state.indexer.write().take() {
        indexer.stop();
    }
    state.db.clear_all().map_err(|e| e.to_string())?;
    for store in [
        &state.vectors,
        &state.media_vectors,
        &state.clip_vectors,
        &state.audio_vectors,
    ] {
        store.read().clear().map_err(|e| format!("清理向量库失败：{e}"))?;
    }
    let _ = state.db.log("index", "已清空全部索引数据");
    Ok(())
}

#[tauri::command]
pub fn data_dir_path(state: State<'_, Arc<AppState>>) -> String {
    state.data_dir.to_string_lossy().to_string()
}

#[derive(Serialize)]
pub struct ExtensionStatusOut {
    pub onnxruntime: bool,
    pub onnxruntime_path: String,
    pub onnxruntime_version: String,
    pub python: bool,
    pub python_path: String,
    pub root: String,
    pub detail: String,
}

fn extension_root(state: &AppState) -> std::path::PathBuf {
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    candidates.push(state.app_dir.clone());
    candidates.push(state.app_dir.join("resources"));
    let mut parent = state.app_dir.clone();
    for _ in 0..4 {
        if let Some(next) = parent.parent() {
            parent = next.to_path_buf();
            candidates.push(parent.clone());
        } else {
            break;
        }
    }
    candidates
        .into_iter()
        .find(|p| p.join(".tools").exists() || p.join("scripts").exists())
        .unwrap_or_else(|| state.app_dir.clone())
}

#[tauri::command]
pub fn extension_status(state: State<'_, Arc<AppState>>) -> ExtensionStatusOut {
    extension_status_for(&state)
}

fn extension_status_for(state: &AppState) -> ExtensionStatusOut {
    let root = extension_root(state);
    // 安装版把 onnxruntime.dll 直接放在 exe 同级（NSIS 资源映射），
    // 便携/开发版则放在 .tools\onnxruntime 下，两种布局都要认。
    let mut ort = root.join(".tools").join("onnxruntime").join("onnxruntime.dll");
    if !ort.is_file() {
        let beside = state.app_dir.join("onnxruntime.dll");
        if beside.is_file() {
            ort = beside;
        }
    }
    let python = root.join(".tools").join("python").join("python.exe");
    let version = ort.file_name()
        .map(|name| ort.with_file_name(format!("{}.version", name.to_string_lossy())))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
        .trim()
        .to_string();
    ExtensionStatusOut {
        onnxruntime: ort.is_file(),
        onnxruntime_path: ort.to_string_lossy().replace('/', "\\"),
        onnxruntime_version: version,
        python: python.is_file(),
        python_path: python.to_string_lossy().replace('/', "\\"),
        root: root.to_string_lossy().replace('/', "\\"),
        detail: String::new(),
    }
}

fn unload_cross_modal_for_extension(state: &AppState) -> Result<(), String> {
    if state.scan_busy.load(Ordering::Relaxed) {
        return Err("扫描或索引运行期间不能更新或删除 ONNX Runtime".into());
    }
    if let Some(indexer) = state.indexer() {
        let progress = indexer.snapshot();
        if progress.running || progress.queued > 0 {
            return Err("索引运行期间不能更新或删除 ONNX Runtime".into());
        }
        indexer.set_cross_modal(None, None);
    }
    *state.clip.write() = None;
    *state.clap.write() = None;
    Ok(())
}

/// Install an optional extension into the project-local `.tools` directory.
/// Only the two fixed script names are accepted; no user-provided command is
/// executed and existing files are never removed.
#[tauri::command]
pub fn extension_install(state: State<'_, Arc<AppState>>, kind: String) -> Result<ExtensionStatusOut, String> {
    let script_name = match kind.as_str() {
        "onnxruntime" => "install-onnxruntime.ps1",
        "python" => "install-python-sidecar.ps1",
        _ => return Err("未知扩展类型".into()),
    };
    let root = extension_root(&state);
    let mut script = root.join("scripts").join(script_name);
    if !script.is_file() {
        script = state.app_dir.join("resources").join("scripts").join(script_name);
    }
    if !script.is_file() {
        return Err(format!("找不到扩展安装脚本：{}", script.display()));
    }
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .current_dir(&root)
        .output()
        .map_err(|e| format!("启动 PowerShell 失败：{e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if msg.is_empty() { format!("扩展安装失败（退出码 {:?}）", output.status.code()) } else { msg });
    }
    let _ = state.db.log("extension", &format!("已安装项目扩展：{kind}"));
    Ok(extension_status_for(&state))
}

#[tauri::command]
pub fn extension_update(state: State<'_, Arc<AppState>>, kind: String) -> Result<ExtensionStatusOut, String> {
    if kind != "onnxruntime" {
        return Err("目前仅支持检查 ONNX Runtime 更新".into());
    }
    let before = extension_status_for(&state);
    if !before.onnxruntime {
        return Err("ONNX Runtime 尚未安装".into());
    }
    unload_cross_modal_for_extension(&state)?;
    let root = extension_root(&state);
    let mut script = root.join("scripts").join("install-onnxruntime.ps1");
    if !script.is_file() {
        script = state.app_dir.join("resources").join("scripts").join("install-onnxruntime.ps1");
    }
    if !script.is_file() {
        return Err(format!("找不到 ONNX Runtime 更新脚本：{}", script.display()));
    }
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-Update")
        .current_dir(&root)
        .output()
        .map_err(|e| format!("启动 ONNX Runtime 更新失败：{e}"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            format!("ONNX Runtime 更新失败（退出码 {:?}）", output.status.code())
        } else {
            msg
        });
    }
    let mut status = extension_status_for(&state);
    if state.app_dir.join("onnxruntime.dll").is_file() {
        let legacy_dll = state.app_dir.join("onnxruntime.dll");
        move_extension_to_recycle_bin(&legacy_dll)?;
        let legacy_version = state.app_dir.join("onnxruntime.dll.version");
        if legacy_version.is_file() {
            move_extension_to_recycle_bin(&legacy_version)?;
        }
    }
    status.detail = if !before.onnxruntime_version.is_empty()
        && before.onnxruntime_version == status.onnxruntime_version
    {
        format!("ONNX Runtime {} 已是最新版", status.onnxruntime_version)
    } else {
        format!("ONNX Runtime 已更新到 {}", status.onnxruntime_version)
    };
    let _ = state.db.log("extension", &status.detail);
    Ok(status)
}

#[cfg(windows)]
fn move_extension_to_recycle_bin(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FileOp {
        hwnd: *mut std::ffi::c_void,
        operation: u32,
        from: *const u16,
        to: *const u16,
        flags: u16,
        aborted: i32,
        mappings: *mut std::ffi::c_void,
        progress_title: *const u16,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHFileOperationW(operation: *mut FileOp) -> i32;
    }

    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]);
    let mut op = FileOp {
        hwnd: std::ptr::null_mut(),
        operation: 3,
        from: from.as_ptr(),
        to: std::ptr::null(),
        flags: 0x0004 | 0x0010 | 0x0040 | 0x0400,
        aborted: 0,
        mappings: std::ptr::null_mut(),
        progress_title: std::ptr::null(),
    };
    let result = unsafe { SHFileOperationW(&mut op) };
    if result != 0 || op.aborted != 0 {
        return Err(format!("无法将扩展移入回收站：{}（错误码 {result}）", path.display()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn move_extension_to_recycle_bin(path: &std::path::Path) -> Result<(), String> {
    Err(format!("当前平台不支持回收站删除：{}", path.display()))
}

#[tauri::command]
pub fn extension_remove(state: State<'_, Arc<AppState>>, kind: String) -> Result<ExtensionStatusOut, String> {
    if kind != "onnxruntime" {
        return Err("目前仅支持删除 ONNX Runtime 扩展".into());
    }
    unload_cross_modal_for_extension(&state)?;
    let root = extension_root(&state);
    let candidates = [
        root.join(".tools").join("onnxruntime").join("onnxruntime.dll"),
        root.join(".tools").join("onnxruntime").join("onnxruntime.dll.version"),
        state.app_dir.join("onnxruntime.dll"),
        state.app_dir.join("onnxruntime.dll.version"),
    ];
    let existing: Vec<_> = candidates.into_iter().filter(|path| path.is_file()).collect();
    if existing.is_empty() {
        return Err("没有找到已安装的 ONNX Runtime".into());
    }
    for path in &existing {
        move_extension_to_recycle_bin(path)?;
    }
    let _ = state.db.log("extension", "已将 ONNX Runtime 扩展移入回收站");
    Ok(extension_status_for(&state))
}
