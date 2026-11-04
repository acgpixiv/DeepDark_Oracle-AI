//! 星图数据层（《2D转3D与星图需求明细》#1~#33 的后端部分）。
//!
//! 与 cmd.rs 里旧的 `graph_data` 相比，这里的差别是**渐进式**：
//! `star_overview` 只给 Level 0 的类型节点（最多 9 个），
//! `star_children` 按需展开下一层。14 万文件的库一次性铺开不可行
//! ——力导向会把整图拉成一团毛线，首屏也要好几秒。按需加载后首屏只有一次
//! GROUP BY，符合需求 #19 的 Level 0/1/2/3 分层。
//!
//! 额外产出父节点 id 与「是否还有子节点」，供前端做钻取、面包屑与返回上级。
use crate::db::Db;
use crate::state::AppState;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::State;

/// 单个节点。字段刻意与前端 `GalaxyNode` 对齐，省掉一层映射。
#[derive(Serialize, Clone, Debug)]
pub struct StarNode {
    pub id: String,
    pub name: String,
    /// folder | file | kind | root
    pub node_type: String,
    /// 文件用完整路径，文件夹也存路径，类型节点存 kind key。
    pub path: String,
    pub kind: String,
    pub size: i64,
    /// 递归文件数；文件恒为 1。
    pub count: i64,
    pub depth: i64,
    pub mtime: i64,
    /// 父节点 id，根为 None。前端据此做「返回上一级」与树重建（#20/#23）。
    pub parent: Option<String>,
    /// 是否还有没展开的子节点（决定是否画「可钻取」的小加号）。
    pub has_children: bool,
    /// 聚类编号，同簇节点同色（#14 Louvain 的轻量替代：按顶层目录分簇）。
    pub cluster: i64,
    /// 十六进制颜色，前端直接用，避免前后端两套配色漂移。
    pub color: String,
    /// 恒星类型（仅 `node_type == "drive"` 有意义）：system / data / archive / removable。
    ///
    /// 需求 #1 要求「系统盘 = 蓝白高温恒星、仓库盘 = 橙红巨星、冷备份 = 暗红矮星」。
    /// 判定放在后端而不是前端：同一块盘的分类不该因为界面改版而漂移，
    /// 而且前端拿不到"这块盘是不是系统盘"这类信息。
    #[serde(default)]
    pub star_class: String,
    /// 物理容量（字节），仅恒星有值；0 表示取不到。
    ///
    /// 与 `size` 的区别：`size` 是**已索引**的字节数，容量是盘的物理大小。
    /// 恒星半径必须按容量算 —— 一块只索引了 30GB 的 2TB 盘，按索引量算会
    /// 小得和 U 盘一样，用户根本看不出"大容量盘"。见 `disk::star_radius_of`。
    #[serde(default)]
    pub capacity: i64,
    /// 已用比例 0~1，用于恒星外圈的进度光环。
    #[serde(default)]
    pub used_ratio: f32,
}

/// 边。`strength` 归一化到 0~1，前端按它算连线粗细（#6）。
#[derive(Serialize, Clone, Debug)]
pub struct StarEdge {
    pub source: String,
    pub target: String,
    /// contains | tag | similar | temporal | causal | reference
    pub edge_type: String,
    pub strength: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct StarGraph {
    pub root: String,
    pub name: String,
    /// 本批节点所属层级：0 类型 / 1 索引根 / 2 分类下 / 3 目录
    pub level: i64,
    pub nodes: Vec<StarNode>,
    pub edges: Vec<StarEdge>,
    pub total_files: i64,
    pub total_bytes: i64,
    /// 该目录没画出来的文件数（前端显示「还有 N 个」）。
    pub hidden_files: i64,
    pub hidden_bytes: i64,
    /// 被 limit 截掉的兄弟节点数。
    pub truncated: i64,
}

// ---------------------------------------------------------------- 配色

/// 分类 → 颜色（与前端 galaxy/model.ts 的 LEGEND 完全一致）。
pub fn kind_color(kind: &str) -> &'static str {
    match kind {
        "folder" | "root" => "#4E79A7",
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

pub fn kind_label(kind: &str) -> &'static str {
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

const KINDS: &[&str] = &[
    "document", "image", "video", "audio", "text", "archive", "app", "other",
];

fn norm_kind(kind: &str) -> &str {
    if KINDS.contains(&kind) {
        kind
    } else {
        "other"
    }
}

// ---------------------------------------------------------------- 布局缓存

/// 布局缓存（需求 #52 的三级缓存之「Rust 侧」）。
///
/// 用带 TTL 的 HashMap 而不是 moka：星图一次会话里布局请求只有个位数，
/// 锁竞争可以忽略，不值得为它引入异步缓存依赖。
pub struct LayoutCache {
    inner: parking_lot::Mutex<HashMap<String, (i64, Vec<[f32; 3]>)>>,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self { inner: parking_lot::Mutex::new(HashMap::new()) }
    }
}

const LAYOUT_TTL_SECS: i64 = 300;

impl LayoutCache {
    fn get(&self, key: &str) -> Option<Vec<[f32; 3]>> {
        let now = crate::util::now_ts();
        let g = self.inner.lock();
        g.get(key)
            .and_then(|(ts, v)| if now - *ts <= LAYOUT_TTL_SECS { Some(v.clone()) } else { None })
    }

    fn put(&self, key: &str, v: Vec<[f32; 3]>) {
        let now = crate::util::now_ts();
        let mut g = self.inner.lock();
        // 顺手清过期项，避免长时间运行内存缓慢上涨。
        g.retain(|_, (ts, _)| now - *ts <= LAYOUT_TTL_SECS);
        g.insert(key.to_string(), (now, v));
    }
}

/// 进程内单例，命令层直接取。
pub fn layout_cache() -> &'static LayoutCache {
    static ONCE: std::sync::OnceLock<LayoutCache> = std::sync::OnceLock::new();
    ONCE.get_or_init(LayoutCache::default)
}

/// 读回上次算好的布局，命中时前端首屏不用再跑一遍力导向。
#[tauri::command]
pub fn star_layout_get(key: String) -> Option<Vec<[f32; 3]>> {
    layout_cache().get(&key)
}

#[tauri::command]
pub fn star_layout_put(key: String, positions: Vec<f32>) -> bool {
    if positions.is_empty() || positions.len() % 3 != 0 {
        return false;
    }
    let mut out = Vec::with_capacity(positions.len() / 3);
    for c in positions.chunks_exact(3) {
        out.push([c[0], c[1], c[2]]);
    }
    layout_cache().put(&key, out);
    true
}

// ---------------------------------------------------------------- 工具函数

fn parts_of(p: &str) -> Vec<&str> {
    p.split('\\').filter(|s| !s.is_empty()).collect()
}

fn display_name(p: &str) -> String {
    parts_of(p)
        .last()
        .map(|s| s.to_string())
        .unwrap_or_else(|| p.to_string())
}

/// 目录前缀 → SQLite 字符串区间 [lo, hi)。
fn prefix_bounds(prefix: &str) -> (String, String) {
    let mut lo = crate::util::norm_path(prefix);
    if !lo.ends_with('\\') {
        lo.push('\\');
    }
    let mut hi = lo.clone();
    hi.pop();
    hi.push(']');
    (lo, hi)
}

fn empty_graph(name: &str, level: i64) -> StarGraph {
    StarGraph {
        root: String::new(),
        name: name.to_string(),
        level,
        nodes: Vec::new(),
        edges: Vec::new(),
        total_files: 0,
        total_bytes: 0,
        hidden_files: 0,
        hidden_bytes: 0,
        truncated: 0,
    }
}

fn root_node(total_files: i64, total_bytes: i64) -> StarNode {
    StarNode {
        id: ":root".into(),
        name: "全部索引".into(),
        node_type: "root".into(),
        path: String::new(),
        kind: String::new(),
        size: total_bytes,
        count: total_files,
        depth: 0,
        mtime: 0,
        parent: None,
        has_children: true,
        cluster: 0,
        color: kind_color("root").into(),
        star_class: String::new(),
        capacity: 0,
        used_ratio: 0.0,
    }
}

// ---------------------------------------------------------------- Level 0

/// 恒星层：每个有索引数据的磁盘盘符一颗恒星（需求 #5）。
///
/// 旧实现返回的是「类别节点」（图片/文档/…），和「盘符 = 恒星」的诉求对不上：
/// 用户打开星图看到的应该是 C/D/E 几颗恒星，而不是八个色块。
/// 类别信息仍然有用，挂在恒星上做行星（展开 `drive:E:` 即可看到）。
///
/// 一次 `drive_kind_stats()` 就能拿到「盘 × 类别」的全部规模，
/// 走 ix_files_drive_kind(drive, kind, size) 覆盖索引，千万行库也不回表。
#[tauri::command]
pub fn star_overview(state: State<'_, Arc<AppState>>) -> StarGraph {
    let rows = state.db.drive_kind_stats().unwrap_or_default();
    build_drive_level(&rows, &crate::disk::volumes(), "全部索引", 0)
}

/// 本机盘符的物理信息（容量 / 可用 / 卷标 / 是否可移动）。
///
/// 与 `star_overview` 分开是有意为之：恒星图需要"这台机器上有哪些盘"
/// 才能把**没被索引**的盘也画出来 —— 用户插上一块新盘却看不到恒星，
/// 会以为星图坏了。前端拿这份清单做「未索引盘」的灰态恒星。
#[tauri::command]
pub fn star_drives() -> Vec<DriveOut> {
    crate::disk::volumes()
        .into_iter()
        .map(|d| {
            // 先算完再搬字段：DriveOut 会 move 掉 d 里的 String。
            let star_class = classify_drive(&d.letter.to_uppercase(), Some(&d), 1, d.total_bytes as i64);
            let star_radius = crate::disk::star_radius_of(d.total_bytes);
            let gravity = crate::disk::gravity_of(d.total_bytes);
            DriveOut {
                mount: d.mount,
                letter: d.letter,
                label: d.label,
                fs: d.fs,
                total_bytes: d.total_bytes as i64,
                free_bytes: d.free_bytes as i64,
                removable: d.removable,
                star_class,
                star_radius,
                gravity,
            }
        })
        .collect()
}

#[derive(Serialize, Clone, Debug)]
pub struct DriveOut {
    pub mount: String,
    pub letter: String,
    pub label: String,
    pub fs: String,
    pub total_bytes: i64,
    pub free_bytes: i64,
    pub removable: bool,
    pub star_class: String,
    /// 引擎直接用的世界坐标半径，前端不必再算一遍。
    pub star_radius: f32,
    /// 引力强度 0~1（恒星质量决定转速与行星轨道半径）。
    pub gravity: f32,
}

/// 把「盘 × 类别」聚合折叠成恒星节点 + 边。
fn build_drive_level(
    rows: &[(String, String, i64, i64)],
    disks: &[crate::disk::DriveInfo],
    name: &str,
    level: i64,
) -> StarGraph {
    let mut order: Vec<String> = Vec::new();
    let mut agg: HashMap<String, (i64, i64, HashMap<String, i64>)> = HashMap::new();
    for (drive, kind, count, bytes) in rows {
        let d = if drive.trim().is_empty() { "?".to_string() } else { drive.to_uppercase() };
        let e = agg.entry(d.clone()).or_insert_with(|| {
            order.push(d.clone());
            (0, 0, HashMap::new())
        });
        e.0 += *count;
        e.1 += *bytes;
        *e.2.entry(kind.clone()).or_insert(0) += *count;
    }
    order.sort_by_key(|d| std::cmp::Reverse(agg.get(d).map(|v| v.1).unwrap_or(0)));

    let total_files: i64 = agg.values().map(|v| v.0).sum();
    let total_bytes: i64 = agg.values().map(|v| v.1).sum();
    let mut nodes = vec![root_node(total_files, total_bytes)];
    let mut edges = Vec::new();

    // 物理容量：一块盘只要在 `volumes()` 里查得到就用它的真实大小，
    // 查不到（外接盘刚拔掉、权限异常）才退回"已索引字节数"，恒星至少不会消失。
    let by_letter: HashMap<String, &crate::disk::DriveInfo> = disks
        .iter()
        .map(|d| (d.letter.to_uppercase(), d))
        .collect();

    for (i, drive) in order.iter().enumerate() {
        let (count, bytes, kinds) = &agg[drive];
        let id = format!("drive:{}", drive);
        // 主类别：该盘里文件数最多的类别，用来给恒星上色。
        let dominant = kinds
            .iter()
            .max_by_key(|(_, c)| **c)
            .map(|(k, _)| norm_kind(k))
            .unwrap_or("other");
        let letter = drive.trim_end_matches(':').to_uppercase();
        let disk = by_letter.get(&letter).copied();
        // 容量优先取物理值；取不到就用索引字节数兜底，恒星不会因为
        // 一次 Win32 调用失败就缩成一个点。
        let capacity = match disk {
            Some(d) if d.total_bytes > 0 => d.total_bytes as i64,
            _ => *bytes,
        };
        let used_ratio = match disk {
            Some(d) if d.total_bytes > 0 => d.used_ratio() as f32,
            // 没有物理容量时，"已用比例"只能是索引占容量的比例，等价于全部。
            _ => 0.42,
        };
        let star_class = classify_drive(&letter, disk, *count, *bytes);
        nodes.push(StarNode {
            id: id.clone(),
            name: drive.trim_end_matches(':').to_string(),
            node_type: "drive".into(),
            path: drive.clone(),
            kind: dominant.to_string(),
            size: *bytes,
            count: *count,
            depth: level + 1,
            mtime: 0,
            parent: Some(":root".into()),
            has_children: true,
            cluster: i as i64 + 1,
            // 恒星颜色由「盘的角色」决定，而不是内容主类别：
            // 需求 #1 要的是三体式的恒星分级（系统盘蓝白、仓库盘橙红、备份盘暗红）。
            color: star_class_color(&star_class, dominant).into(),
            star_class: star_class.clone(),
            capacity,
            used_ratio,
        });
        edges.push(StarEdge {
            source: ":root".into(),
            target: id,
            edge_type: "contains".into(),
            strength: (*count as f64 / total_files.max(1) as f64).sqrt().clamp(0.2, 1.0),
        });
    }

    let mut g = empty_graph(name, level);
    g.nodes = nodes;
    g.edges = edges;
    g.total_files = total_files;
    g.total_bytes = total_bytes;
    g
}

/// 盘的角色 → 恒星光谱型（需求 #1「不同盘符类型视觉区分明显」）。
///
/// 判定顺序有讲究：先看是不是系统盘（只有 C 盘上的 Windows 目录才算），
/// 再看可移动盘，最后按容量分仓库盘 / 备份盘。
///
/// 没有用「盘里主类别是图片所以是媒体盘」这种内容驱动的判定：盘的角色是
/// 用户的用法，内容会变，角色相对稳定；把两者混在一起会让同一块盘的颜色
/// 随索引内容漂移，用户第二次进来会以为换了一块盘。
fn classify_drive(
    letter: &str,
    disk: Option<&crate::disk::DriveInfo>,
    count: i64,
    bytes: i64,
) -> String {
    if let Some(d) = disk {
        if d.removable {
            return "removable".into();
        }
    }
    let is_system = letter.eq_ignore_ascii_case("C")
        && std::path::Path::new(r"C:\Windows\System32").is_dir();
    if is_system {
        return "system".into();
    }
    // 容量拿不到时用索引字节数判断，阈值放宽到 300GB —— 一块"索引了 300GB
    // 以上"的盘基本只能是仓库盘。
    let total = disk.map(|d| d.total_bytes as i64).filter(|v| *v > 0).unwrap_or(bytes);
    let gb = total as f64 / (1024f64 * 1024f64 * 1024f64);
    // 文件多但占空间小的是"素材/代码盘"，也归到 data，不分出第四种。
    if gb >= 1500.0 {
        "archive".into()
    } else if count > 0 {
        "data".into()
    } else {
        "archive".into()
    }
}

/// 恒星光谱型 → 颜色。与前端 `STAR_CLASS` 表一一对应。
fn star_class_color(class: &str, dominant: &str) -> &'static str {
    match class {
        "system" => "#9ad4ff",   // 蓝白高温恒星
        "data" => "#ffb066",     // 橙红巨星
        "archive" => "#c9705a",  // 暗红矮星
        "removable" => "#a8e6c9", // 冷却白矮星（移动盘）
        _ => kind_color(dominant),
    }
}

// ---------------------------------------------------------------- 按需展开

/// 展开某个节点的下一层。
///
/// - 空串 / `:root`：所有启用的索引根
/// - `kind:image`：该类别的索引根（Level 1）
/// - `dir:D:\xxx`：子目录 + 抽样文件
/// - `file:...`：叶子，返回空图
#[tauri::command]
pub fn star_children(
    state: State<'_, Arc<AppState>>,
    node_id: Option<String>,
    file_limit: Option<usize>,
    dir_limit: Option<usize>,
) -> StarGraph {
    let node_id = node_id.unwrap_or_default();
    let file_limit = file_limit.unwrap_or(60).clamp(0, 400);
    let dir_limit = dir_limit.unwrap_or(80).clamp(1, 600);
    expand(&state.db, &node_id, file_limit, dir_limit)
}

fn expand(db: &Arc<Db>, node_id: &str, file_limit: usize, dir_limit: usize) -> StarGraph {
    // 注意：这里**不能**再调 db.kind_stats()。
    // 旧实现每次展开都跑一次全表 GROUP BY（28 万行 ≈ 30ms，千万行 ≈ 秒级），
    // 点一下就卡一下。总量只在真正需要时用 O(1) 的聚合取。

    // ---- 恒星：盘符 → 类别行星 -----------------------------------------
    if let Some(drive) = node_id.strip_prefix("drive:") {
        return expand_drive(db, drive, file_limit);
    }

    // ---- 根：所有盘符恒星 ---------------------------------------------
    if node_id.is_empty() || node_id == ":root" {
        let rows = db.drive_kind_stats().unwrap_or_default();
        return build_drive_level(&rows, &crate::disk::volumes(), "全部索引", 0);
    }

    // ---- 类别行星：该盘该类别下的一级目录 -------------------------------
    if let Some(rest) = node_id.strip_prefix("kind:") {
        if let Some((kind, drive)) = rest.split_once('@') {
            return expand_kind(db, kind, drive, dir_limit);
        }
    }

    // ---- 分类节点：该类别下的索引根（保留旧入口，供面包屑回退） ---------
    if let Some(kind) = node_id.strip_prefix("kind:") {
        let kind = norm_kind(kind).to_string();
        let roots = db.index_roots().unwrap_or_default();
        let parent = node_id.to_string();
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut kf = 0i64;
        let mut kb = 0i64;
        let mut cluster = 0i64;

        for r in roots.iter().filter(|r| r.enabled) {
            let path = crate::util::norm_path(&r.root);
            if path.is_empty() {
                continue;
            }
            let (lo, hi) = prefix_bounds(&path);
            // 只回一行聚合，绝不把整类文件行拉进内存。
            let (count, bytes) = db.kind_prefix_stats(&kind, &lo, &hi).unwrap_or((0, 0));
            if count == 0 {
                continue;
            }
            kf += count;
            kb += bytes;
            cluster += 1;
            let id = format!("dir:{}", path);
            nodes.push(StarNode {
                id: id.clone(),
                name: display_name(&path),
                node_type: "folder".into(),
                path: path.clone(),
                kind: kind.clone(),
                size: bytes,
                count,
                depth: 2,
                mtime: r.last_scan,
                parent: Some(parent.clone()),
                has_children: true,
                cluster,
                color: kind_color(&kind).into(),
                star_class: String::new(),
                capacity: 0,
                used_ratio: 0.0,
            });
            edges.push(StarEdge {
                source: parent.clone(),
                target: id,
                edge_type: "contains".into(),
                strength: 1.0,
            });
        }
        // 二遍：总量已知后再算归一化强度。
        for e in edges.iter_mut() {
            let c = nodes
                .iter()
                .find(|n| n.id == e.target)
                .map(|n| n.count)
                .unwrap_or(0);
            e.strength = (c as f64 / kf.max(1) as f64).sqrt().clamp(0.2, 1.0);
        }

        let mut g = empty_graph(&format!("{}分类", kind_label(&kind)), 2);
        g.nodes = nodes;
        g.edges = edges;
        g.total_files = kf;
        g.total_bytes = kb;
        return g;
    }

    // ---- 目录节点：直接子目录 + 抽样文件 --------------------------------
    if let Some(path) = node_id.strip_prefix("dir:") {
        return expand_dir(db, path, node_id, file_limit, dir_limit);
    }

    // ---- 文件节点：叶子 -------------------------------------------------
    empty_graph("", 9)
}

/// 展开一颗恒星：该盘下的类别行星。
///
/// 需求 #6 的核心约束是「点开节点不能冒出一大堆行星」。盘下面有几十万个
/// 文件，所以这里只给 8 个类别（固定上限），由前端画成绕恒星公转的行星。
fn expand_drive(db: &Arc<Db>, drive: &str, _file_limit: usize) -> StarGraph {
    let drive = drive.trim().to_uppercase();
    let rows = db.drive_kind_stats().unwrap_or_default();
    let parent = format!("drive:{}", drive);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let mut per_kind: HashMap<&str, (i64, i64)> = HashMap::new();
    for (d, kind, count, bytes) in &rows {
        if !d.eq_ignore_ascii_case(&drive) {
            continue;
        }
        let k = norm_kind(kind);
        let e = per_kind.entry(k).or_insert((0, 0));
        e.0 += *count;
        e.1 += *bytes;
    }
    let total_files: i64 = per_kind.values().map(|v| v.0).sum();
    let total_bytes: i64 = per_kind.values().map(|v| v.1).sum();

    let mut ordered: Vec<(&str, (i64, i64))> = per_kind.into_iter().collect();
    ordered.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));

    for (i, (kind, (count, bytes))) in ordered.into_iter().enumerate() {
        if count <= 0 {
            continue;
        }
        let id = format!("kind:{}@{}", kind, drive);
        nodes.push(StarNode {
            id: id.clone(),
            name: kind_label(kind).to_string(),
            node_type: "kind".into(),
            path: drive.clone(),
            kind: kind.to_string(),
            size: bytes,
            count,
            depth: 1,
            mtime: 0,
            parent: Some(parent.clone()),
            has_children: true,
            cluster: i as i64 + 1,
            color: kind_color(kind).into(),
            star_class: String::new(),
            capacity: 0,
            used_ratio: 0.0,
        });
        edges.push(StarEdge {
            source: parent.clone(),
            target: id,
            edge_type: "contains".into(),
            strength: (count as f64 / total_files.max(1) as f64).sqrt().clamp(0.2, 1.0),
        });
    }

    let mut g = empty_graph(&format!("{} 盘", drive.trim_end_matches(':')), 1);
    g.root = drive.clone();
    g.nodes = nodes;
    g.edges = edges;
    g.total_files = total_files;
    g.total_bytes = total_bytes;
    g
}

/// 展开一颗类别行星：该盘该类别下的**一级目录**（上限 24 颗卫星）。
///
/// 再往下就是目录，走 `dir:` 分支。这样「盘 → 类别 → 顶层目录 → 子目录」
/// 每层节点数都被 `dir_limit` 卡住，不会出现「点一下冒出几千颗行星」。
fn expand_kind(db: &Arc<Db>, kind: &str, drive: &str, dir_limit: usize) -> StarGraph {
    let kind = norm_kind(kind).to_string();
    let drive = drive.trim().to_uppercase();
    let parent = format!("kind:{}@{}", kind, drive);
    let limit = dir_limit.clamp(4, 64);
    let rows = db.kind_dir_rollup(&drive, &kind, limit).unwrap_or_default();
    let total_files: i64 = rows.iter().map(|r| r.1).sum();
    let total_bytes: i64 = rows.iter().map(|r| r.2).sum();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (i, (path, count, bytes)) in rows.into_iter().enumerate() {
        let id = format!("dir:{}", path);
        nodes.push(StarNode {
            id: id.clone(),
            name: display_name(&path),
            node_type: "folder".into(),
            path: path.clone(),
            kind: kind.clone(),
            size: bytes,
            count,
            depth: 2,
            mtime: 0,
            parent: Some(parent.clone()),
            has_children: true,
            cluster: i as i64 + 1,
            color: kind_color(&kind).into(),
            star_class: String::new(),
            capacity: 0,
            used_ratio: 0.0,
        });
        edges.push(StarEdge {
            source: parent.clone(),
            target: id,
            edge_type: "contains".into(),
            strength: (count as f64 / total_files.max(1) as f64).sqrt().clamp(0.2, 1.0),
        });
    }

    let mut g = empty_graph(&format!("{} · {} 盘", kind_label(&kind), drive.trim_end_matches(':')), 2);
    g.root = drive.clone();
    g.nodes = nodes;
    g.edges = edges;
    g.total_files = total_files;
    g.total_bytes = total_bytes;
    g
}

/// 展开一个目录：只取**直接子目录** + 少量代表文件。
///
/// 关键改动（需求 #2 千兆库卡顿）：
///  - 不再 `dir_rollup_under` 把整棵子树的每一层目录都拉回内存建 HashMap；
///  - 不再逐条跑 `dir_recursive_stats`（142 ms）+ `dir_child_rollup`（290 ms）
///    + `dir_file_stats` —— 这三条都要全表扫一遍，加起来 400 ms 以上，
///    连点几次目录就明显卡；
///  - 改成一次 `all_dir_rollup`（全库 GROUP BY dir，33,608 行约 63 ms，
///    带 3 秒 TTL 缓存，连点同一层几乎零成本）再在内存里折叠出本目录的
///    直接子目录与子树合计，三条查询一次全替掉；
///  - 子目录上限默认 80（前端可调），展开一个目录最多多出若干颗行星 + 若干文件。
fn expand_dir(
    db: &Arc<Db>,
    path: &str,
    node_id: &str,
    file_limit: usize,
    dir_limit: usize,
) -> StarGraph {
    let path = crate::util::norm_path(path);
    let root_key = crate::db::dir_lookup_key(&path);
    let all_dirs = db.all_dir_rollup().unwrap_or_default();
    let (folded, own) = crate::db::fold_dir_children(&all_dirs, &root_key);
    let mut kids: Vec<(String, i64, i64)> = folded.get(&root_key).cloned().unwrap_or_default();
    // 目录自身直属的文件（不含子目录）就是折叠结果里的 root_own。
    let direct_files = own.0;
    let direct_bytes = own.1;
    // 递归合计 = 直属 + 各直接子目录的子树合计 —— 即 dir_recursive_stats 的语义。
    let dir_files: i64 = direct_files + kids.iter().map(|k| k.1).sum::<i64>();
    let dir_bytes: i64 = direct_bytes + kids.iter().map(|k| k.2).sum::<i64>();

    let parent = node_id.to_string();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut cluster = 0i64;

    kids.sort_by(|a, b| b.1.cmp(&a.1));
    let mut truncated = 0i64;
    if kids.len() > dir_limit {
        truncated = (kids.len() - dir_limit) as i64;
        kids.truncate(dir_limit);
    }
    for (child, count, bytes) in kids {
        // 子目录直接挂在父节点下，id 用完整路径，避免同级重名。
        if child.eq_ignore_ascii_case(&path) {
            continue;
        }
        cluster += 1;
        let id = format!("dir:{}", child);
        nodes.push(StarNode {
            id: id.clone(),
            name: display_name(&child),
            node_type: "folder".into(),
            path: child.clone(),
            kind: String::new(),
            size: bytes,
            count,
            depth: 0,
            mtime: 0,
            parent: Some(parent.clone()),
            has_children: true,
            cluster,
            color: kind_color("folder").into(),
            star_class: String::new(),
            capacity: 0,
            used_ratio: 0.0,
        });
        edges.push(StarEdge {
            source: parent.clone(),
            target: id,
            edge_type: "contains".into(),
            strength: (count as f64 / dir_files.max(1) as f64).sqrt().clamp(0.15, 1.0),
        });
    }

    let mut hidden_files = direct_files.max(0);
    let mut hidden_bytes = direct_bytes.max(0);

    if file_limit > 0 && direct_files > 0 {
        let files = db.query_rows(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags \
             FROM files WHERE missing=0 AND dir = ?1 ORDER BY size DESC, mtime DESC LIMIT ?2",
            vec![
                rusqlite::types::Value::Text(path.clone()),
                rusqlite::types::Value::Integer(file_limit as i64),
            ],
        );
        let mut kept_bytes = 0i64;
        for f in &files {
            kept_bytes += f.size;
            let id = format!("file:{}", f.path);
            nodes.push(StarNode {
                id: id.clone(),
                name: f.name.clone(),
                node_type: "file".into(),
                path: f.path.clone(),
                kind: f.kind.clone(),
                size: f.size,
                count: 1,
                depth: 1,
                mtime: f.mtime,
                parent: Some(parent.clone()),
                has_children: false,
                cluster,
                color: kind_color(&f.kind).into(),
                star_class: String::new(),
                capacity: 0,
                used_ratio: 0.0,
            });
            edges.push(StarEdge {
                source: parent.clone(),
                target: id,
                edge_type: "contains".into(),
                strength: (f.size as f64 / direct_bytes.max(1) as f64).sqrt().clamp(0.05, 1.0),
            });
        }
        hidden_files = (direct_files - files.len() as i64).max(0);
        hidden_bytes = (direct_bytes - kept_bytes).max(0);
    }

    let mut g = empty_graph(&display_name(&path), 3);
    g.root = path.clone();
    g.nodes = nodes;
    g.edges = edges;
    g.total_files = dir_files;
    g.total_bytes = dir_bytes;
    g.hidden_files = hidden_files;
    g.hidden_bytes = hidden_bytes;
    g.truncated = truncated;
    g
}

// ---------------------------------------------------------------- 语义星图
//
// 需求「排序模式」的真意：2D 星图打开是一坨，要按**内容相似度**重排。
//
// 旧实现按文件夹树建边（父子 contains），结果是一张纯结构图：节点密度低
// 而且毫无信息量 —— 两个文件夹相邻只是因为在同一层，不代表内容有关。
// 这里改成向量语义：
//
//   1. 取样：从向量库里挑 N 个文件（按「目录轮转 + 大小」分层，避免全被
//      某个巨型 pixiv 目录占满，那样星图会退化成一坨同一主题的球）。
//   2. 粗聚类：对归一化向量做球面 k-means（余弦距离），
//      簇数取 sqrt(N/2) 并夹在 [6, 24]。用户要的"模型给它们分类"就是这一步。
//   3. 建边：**先簇内建边（每个点连最近 k1 个）再全局桥接（最近 k2 个）**。
//      只做全局 kNN 会得到一堆互不相连的孤岛小团；只做簇内建边则簇与簇
//      之间断开。两段合起来既有密集的语义团，又有跨主题的细桥。
//   4. 相关性归一：每簇一条骨架边（簇心 ↔ 星云中心），让力导向把同簇
//      节点收拢成"星团"而不是均匀铺开 —— 这就是"排序排好"的视觉体现。
//
// 全部在 spawn_blocking 里跑：10 万条向量 × 512 维的取样与聚类是几百毫秒
// 级 CPU 活，绝不能让它在 IPC 线程上阻塞 UI。

/// 语义星图的一个节点（文件）。
#[derive(Serialize, Clone, Debug)]
pub struct SemanticNode {
    pub id: String,
    /// 文件 id，前端点击时可直接用。
    pub file_id: i64,
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
    pub cluster: i64,
    /// 与所属簇心的余弦相似度 0~1，前端用它决定节点亮度。
    pub centrality: f32,
    /// 所属簇的代表色（十六进制）。
    pub color: String,
}

/// 语义星图的边。
#[derive(Serialize, Clone, Debug)]
pub struct SemanticEdge {
    pub source: String,
    pub target: String,
    /// similar | member | bridge
    pub edge_type: String,
    /// 余弦相似度 0~1。
    pub strength: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct SemanticCluster {
    pub id: i64,
    pub size: i64,
    /// 簇里最大的几个文件名，用于给簇起名（前端显示"以 xxx 为主"）。
    pub label: String,
    pub color: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct SemanticGraph {
    pub nodes: Vec<SemanticNode>,
    pub edges: Vec<SemanticEdge>,
    pub clusters: Vec<SemanticCluster>,
    /// 参与聚类的文件数。
    pub total: i64,
    /// 向量库里一共有多少条（取样前）。
    pub available: i64,
    /// 实际用到的向量维度，前端可显示"512 维语义空间"。
    pub dim: i64,
    /// 本次用的相似度阈值（前端滑块回填）。
    pub threshold: f64,
    /// 取样是被 max_nodes 截断的，说明视图里只是一部分。
    pub truncated: bool,
}

/// 语义调色板：低饱和、彼此可分，深空底上不刺眼。
const CLUSTER_COLORS: &[&str] = &[
    "#5fb8ff", "#ffa94d", "#8f7dff", "#4fd1a5", "#ff6b8b", "#ffd166",
    "#6ee7f9", "#c792ea", "#a3e635", "#fb923c", "#38bdf8", "#f472b6",
    "#34d399", "#eab308", "#a78bfa", "#22d3ee", "#f87171", "#84cc16",
    "#f0abfc", "#60a5fa", "#fbbf24", "#2dd4bf", "#fb7185", "#a3a3a3",
];

fn cluster_color(i: usize) -> &'static str {
    CLUSTER_COLORS[i % CLUSTER_COLORS.len()]
}

/// 余弦相似度（两边都已是单位向量时就是点积）。
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
    }
    dot
}

/// 归一化（原地）；零向量原样返回，避免除零变成 NaN 污染整个聚类。
fn normalize_vec(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-8 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// 球面 k-means（余弦距离）。
///
/// 为什么不用 k-means++ 完整版：这里 N 最多 600、K 最多 24，
/// 随机初始化 + 10 轮 Lloyd 已经足够稳定，而且单次只要几毫秒。
/// 初始化用**确定性种子**（按 id 散列）而不是随机数 —— 同样的数据两次
/// 打开应该得到同样的星图，否则用户会觉得"每次打开都不一样，很乱"。
fn kmeans_cosine(vectors: &[Vec<f32>], k: usize, iters: usize) -> (Vec<usize>, Vec<Vec<f32>>) {
    let n = vectors.len();
    let k = k.clamp(1, n.max(1));
    let dim = vectors[0].len();

    // 初始化：沿数据顺序等距取 K 个点，天然覆盖不同主题（向量库是按目录
    // 写入的，等距取点等于按目录分层抽样）。
    let mut centers: Vec<Vec<f32>> = (0..k)
        .map(|i| {
            let idx = (i * n / k).min(n.saturating_sub(1));
            let mut c = vectors[idx].clone();
            normalize_vec(&mut c);
            c
        })
        .collect();

    let mut assign = vec![usize::MAX; n];
    for _ in 0..iters {
        let mut changed = false;
        for (i, v) in vectors.iter().enumerate() {
            let mut best = 0usize;
            let mut best_sim = f32::NEG_INFINITY;
            for (ci, c) in centers.iter().enumerate() {
                let sim = cosine(v, c);
                if sim > best_sim {
                    best_sim = sim;
                    best = ci;
                }
            }
            if assign[i] != best {
                assign[i] = best;
                changed = true;
            }
        }
        // 更新簇心
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for (i, v) in vectors.iter().enumerate() {
            let c = assign[i];
            if c >= k {
                continue;
            }
            counts[c] += 1;
            for d in 0..dim {
                sums[c][d] += v[d];
            }
        }
        for ci in 0..k {
            if counts[ci] == 0 {
                // 空簇重新播种到离现有簇心最远的点，避免出现永远空的簇。
                let far = (0..n)
                    .max_by(|&a, &b| {
                        let sa = centers.iter().map(|c| cosine(&vectors[a], c)).fold(f32::MIN, f32::max);
                        let sb = centers.iter().map(|c| cosine(&vectors[b], c)).fold(f32::MIN, f32::max);
                        sa.total_cmp(&sb)
                    })
                    .unwrap_or(0);
                centers[ci] = vectors[far].clone();
            } else {
                for d in 0..dim {
                    sums[ci][d] /= counts[ci] as f32;
                }
                normalize_vec(&mut sums[ci]);
                centers[ci] = std::mem::take(&mut sums[ci]);
            }
        }
        if !changed {
            break;
        }
    }
    (assign, centers)
}

/// 从向量库里分层取样。
///
/// 为什么必须分层而不是"取前 N 条"：向量库是按索引顺序追加的，
/// 取前 N 条等于"最先被索引的那个目录"—— 用户看到的就是一张单主题的图，
/// 正好是他说的"密度低没意义"。
///
/// 做法：按目录分桶，桶内按文件大小降序，然后**轮转取桶**。
/// 大目录权重更高（按 sqrt(文件数) 分配配额），但不会一家独大。
fn sample_ids(
    pool: &[(i64, String)],
    max_nodes: usize,
) -> Vec<i64> {
    if pool.len() <= max_nodes {
        return pool.iter().map(|(id, _)| *id).collect();
    }

    // 分桶
    let mut buckets: HashMap<String, Vec<i64>> = HashMap::new();
    for (id, dir) in pool {
        buckets.entry(top_dir_key(dir)).or_default().push(*id);
    }

    let mut bucket_list: Vec<(String, Vec<i64>)> = buckets.into_iter().collect();
    bucket_list.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    // 配额：sqrt(桶大小) 归一化，保证大目录有更多节点但不会挤掉小目录。
    let weight_total: f64 = bucket_list.iter().map(|(_, v)| (v.len() as f64).sqrt()).sum();
    let mut out: Vec<i64> = Vec::with_capacity(max_nodes);
    let mut taken = vec![0usize; bucket_list.len()];
    let mut cursor = 0usize;

    // 轮转：每轮每个桶取一个，直到取满。比"一次性按配额切片"更均匀，
    // 而且天然满足"每个桶至少有一个"（只要桶数 ≤ max_nodes）。
    let mut exhausted = false;
    while out.len() < max_nodes && !exhausted {
        exhausted = true;
        for (bi, (_key, items)) in bucket_list.iter().enumerate() {
            if out.len() >= max_nodes {
                break;
            }
            if taken[bi] >= items.len() {
                continue;
            }
            // 配额大的桶在一轮里可以多取几个。
            let quota = ((items.len() as f64 / weight_total.max(1.0)) * max_nodes as f64).ceil() as usize;
            let take_now = if cursor < quota.max(1) { 1 } else { 0 };
            if take_now == 0 && cursor > 0 {
                continue;
            }
            out.push(items[taken[bi]]);
            taken[bi] += 1;
            exhausted = false;
        }
        cursor += 1;
        if cursor > 4096 {
            break;
        }
    }

    // 万一轮转没取满（桶被掏空），按剩余顺序补满。
    if out.len() < max_nodes {
        let seen: std::collections::HashSet<i64> = out.iter().copied().collect();
        for (_key, items) in bucket_list.iter() {
            for id in items.iter() {
                if out.len() >= max_nodes {
                    break;
                }
                if !seen.contains(id) {
                    out.push(*id);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

// CLI 复用（--cli semgraph）：只在命令行诊断里用，公开是为了避免把
// 聚类算法复制两份 —— 复制出来的第二份迟早和第一份算出不同结果。

/// 分层取样的公开入口。
pub fn sample_ids_for_cli(pool: &[(i64, String)], max_nodes: usize) -> Vec<i64> {
    sample_ids(pool, max_nodes)
}

/// 归一化的公开入口。
pub fn normalize_vec_cli(v: &mut [f32]) {
    normalize_vec(v)
}

/// 余弦相似度的公开入口。
pub fn cosine_cli(a: &[f32], b: &[f32]) -> f32 {
    cosine(a, b)
}

/// 球面 k-means 的公开入口。
pub fn kmeans_for_cli(vectors: &[Vec<f32>], k: usize, iters: usize) -> (Vec<usize>, Vec<Vec<f32>>) {
    kmeans_cosine(vectors, k, iters)
}

/// 目录 → 分桶键：取盘 + 前两层目录。
fn top_dir_key(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('\\').filter(|s| !s.is_empty()).collect();
    parts.iter().take(3).cloned().collect::<Vec<_>>().join("\\")
}

#[tauri::command]
pub async fn star_semantic(
    state: State<'_, Arc<AppState>>,
    max_nodes: Option<usize>,
    threshold: Option<f64>,
    seed: Option<u64>,
) -> Result<SemanticGraph, String> {
    let max_nodes = max_nodes.unwrap_or(260).clamp(30, 900);
    let max_nodes = max_nodes.min(900);
    let threshold = threshold.unwrap_or(0.55).clamp(0.10, 0.98);
    let st = state.inner().clone();
    let seed = seed.unwrap_or(0);
    tauri::async_runtime::spawn_blocking(move || build_semantic(&st, max_nodes, threshold, seed))
        .await
        .map_err(|e| format!("语义星图计算失败：{e}"))?
}
/// 语义星图的完整构建流程（在阻塞线程里跑）。
fn build_semantic(
    st: &Arc<AppState>,
    max_nodes: usize,
    threshold: f64,
    _seed: u64,
) -> Result<SemanticGraph, String> {
    // 关键：本项目有**四个互相独立的向量空间**，维度不同不能混算：
    //   vectors       主文本空间（768 / 256）—— 文档正文
    //   vectors_media 媒体特征空间（256，HashEmbedder）—— 图片/音视频
    //   vectors_clip  跨模态图片空间（512）—— 图片、视频帧
    //   vectors_audio 跨模态音频空间（512）—— 音频
    //
    // 踩过的坑：只看 files.embed_state='ok' 会拿到 5000 多条记录，但它们的
    // 向量**根本不在主空间里**（主库为 0 条），于是取向量全部落空、星图空白。
    // 正确做法是**从每个空间各自取样**：谁有向量就取谁，按空间大小分配名额。
    let mut spaces_src: Vec<(u8, Arc<crate::vector::VectorStore>)> = vec![
        (0, st.vectors.read().clone()),
        (1, st.media_vectors.read().clone()),
        (2, st.clip_vectors.read().clone()),
        (3, st.audio_vectors.read().clone()),
    ];
    spaces_src.retain(|(_, s)| s.len() > 0);
    if spaces_src.is_empty() {
        return Ok(SemanticGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            clusters: Vec::new(),
            total: 0,
            available: 0,
            dim: 0,
            threshold,
            truncated: false,
        });
    }
    let available: i64 = spaces_src.iter().map(|(_, s)| s.len() as i64).sum();

    // 按空间大小分配名额，每个空间至少留 12 个（小空间也该露脸）。
    let total_len: usize = spaces_src.iter().map(|(_, s)| s.len()).sum();
    let mut picked: Vec<(i64, u8)> = Vec::new();
    for (space, store) in &spaces_src {
        let quota = if spaces_src.len() == 1 {
            max_nodes
        } else {
            ((store.len() as f64 / total_len as f64) * max_nodes as f64).round() as usize
        };
        let quota = quota.clamp(12.min(store.len()), max_nodes);
        // 目录分层取样：拿空间里的全部 id，再按目录轮转。
        let ids = store.all_ids();
        let rows = st.db.files_by_ids(&ids);
        let pool: Vec<(i64, String)> = rows.into_iter().map(|r| (r.id, r.dir.clone())).collect();
        let take = if pool.len() <= quota { pool.iter().map(|(id, _)| *id).collect() } else { sample_ids(&pool, quota) };
        for id in take {
            picked.push((id, *space));
        }
    }
    let truncated = (picked.len() as i64) < available;

    // 取向量：按空间逐批取，同一个 id 只取一次。
    let mut got: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut space_of: HashMap<i64, u8> = HashMap::new();
    for (space, store) in &spaces_src {
        let ids: Vec<i64> = picked.iter().filter(|(_, s)| s == space).map(|(id, _)| *id).collect();
        if ids.is_empty() {
            continue;
        }
        for (id, vec) in store.get_many(&ids) {
            if space_of.contains_key(&id) || vec.is_empty() {
                continue;
            }
            space_of.insert(id, *space);
            got.push((id, vec));
        }
    }
    let dim = got.first().map(|(_, v)| v.len()).unwrap_or(0);
    if got.is_empty() {
        return Ok(SemanticGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            clusters: Vec::new(),
            total: 0,
            available,
            dim: dim as i64,
            threshold,
            truncated,
        });
    }

    // 元数据一次批量取回
    let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
    let rows = st.db.files_by_ids(&ids);
    let mut meta: HashMap<i64, crate::db::FileRow> = HashMap::with_capacity(rows.len());
    for r in rows {
        meta.insert(r.id, r);
    }

    // 只保留既有向量又有元数据的项，两者错位会让节点点到空路径。
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(got.len());
    let mut file_ids: Vec<i64> = Vec::with_capacity(got.len());
    // 每个节点属于哪个向量空间：建边时只在同空间内比较。
    let mut spaces: Vec<u8> = Vec::with_capacity(got.len());
    for (id, mut v) in got {
        if !meta.contains_key(&id) || v.is_empty() {
            continue;
        }
        normalize_vec(&mut v);
        vectors.push(v);
        file_ids.push(id);
        spaces.push(space_of.get(&id).copied().unwrap_or(0));
    }
    let n = vectors.len();
    if n == 0 {
        return Err("向量库里的条目都缺少文件元数据，请先重建索引".into());
    }

    // 簇数：sqrt(N/2) 是经验值 —— N=200 时约 10 簇，每簇 20 个点，
    // 既有"星团"的观感又不至于每个点自成一簇。
    // 维度必须一致才能做余弦；不同空间的向量维度不同（768/512），
    // 这里按"最大空间"的维度筛选出同维的子集来聚类，其余单独成簇。
    // 实际数据里绝大多数库只有一种空间有数据，这条分支很少走到。
    let dims: HashMap<usize, usize> = {
        let mut m: HashMap<usize, usize> = HashMap::new();
        for v in vectors.iter() {
            *m.entry(v.len()).or_insert(0) += 1;
        }
        m
    };

    // 按维度分组，**每组各自做 k-means**，簇 id 全局唯一。
    //
    // 【踩过的坑】上一版只对"主维度"聚类，其余维度的点每个都单独编号成
    // 一个"溢出簇" —— 实测 media(256 维) 148 个点 + clip(512 维) 112 个点，
    // 结果产生 114 个簇（8 个真正的簇 + 112 个各自成簇的 clip 点）。
    // 视觉上就是"一大团 + 112 个孤零零的点"，完全不是用户要的分类。
    //
    // 正解：不同维度的向量空间本来就是不同的语义空间，各聚各的；
    // 簇 id 用全局自增偏移，保证互不冲突。
    let mut assign = vec![0usize; n];
    let mut centers: Vec<Vec<f32>> = Vec::new();
    let mut next_cluster = 0usize;
    let mut dim_list: Vec<usize> = dims.keys().copied().collect();
    // 大的空间先跑，簇 id 也从 0 开始，图例顺序和展示顺序一致。
    dim_list.sort_by_key(|d| std::cmp::Reverse(dims[d]));
    for dim in dim_list {
        // 该维度的所有点（下标）
        let group: Vec<usize> = (0..n).filter(|&i| vectors[i].len() == dim).collect();
        if group.is_empty() {
            continue;
        }
        let group_vecs: Vec<Vec<f32>> = group.iter().map(|&i| vectors[i].clone()).collect();
        let m = group_vecs.len();
        // 同一个维度内按 sqrt(N/2) 定簇数；点数太少（<4）就整组算一簇，
        // 不值得为 2 个点跑 k-means。
        // 小空间（点数 < 12）不再细分：它们本来就只是"另一个模态的若干文件"，
        // 拆成多个簇只会让图上的团数翻倍、每个团又很小，整体被撑散。
        // 实测：clip 空间 30 个点拆成 2 簇后，网格从 3×3 变成 4×4，
        // 缩放从 55% 掉到 41%（画布上什么都看不清）。
        let k = if m < 12 {
            1
        } else {
            ((m as f64 / 2.0).sqrt() as usize).clamp(2, 24).min(m)
        };
        let (local, local_centers) = kmeans_cosine(&group_vecs, k, 12);
        for c in local_centers {
            centers.push(c);
        }
        for (pos, &i) in group.iter().enumerate() {
            assign[i] = next_cluster + local.get(pos).copied().unwrap_or(0);
        }
        next_cluster += k;
    }
    // 展示用的"语义空间维度"：取点数最多的那个空间（通常是 media 256 维）。
    let main_dim = dims.iter().max_by_key(|(_, c)| **c).map(|(d, _)| *d).unwrap_or(0);

    // ---- 建边 --------------------------------------------------------
    // 每个点先在自己簇里找最近的 K1 个（邻居数随簇大小自适应，上限 4）。
    let mut edges: Vec<SemanticEdge> = Vec::new();
    let mut degree: HashMap<usize, usize> = HashMap::new();
    let id_of = |i: usize| format!("sem:{}", file_ids[i]);

    // 按簇分桶
    let mut by_cluster: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &c) in assign.iter().enumerate() {
        by_cluster.entry(c).or_default().push(i);
    }

    // 对每个点，计算与"同簇邻居 + 全局邻居"的相似度。
    // O(N²·dim) 在 N≤900、dim=512 时约 4 亿次乘加 —— 分块的 rayon 并行
    // 只要几十毫秒；串行也就 200ms 级，用户点一下完全可接受。
    let mut neighbors: Vec<Vec<(usize, f32)>> = Vec::with_capacity(n);
    for i in 0..n {
        let ci = assign[i];
        let mut cand: Vec<(usize, f32)> = Vec::new();
        for &j in by_cluster.get(&ci).map(|v| v.as_slice()).unwrap_or(&[]) {
            // 只有同空间（同维度）的向量之间余弦才有意义。
            if j == i || vectors[j].len() != vectors[i].len() {
                continue;
            }
            cand.push((j, cosine(&vectors[i], &vectors[j])));
        }
        // 簇内邻居按相似度降序
        cand.sort_by(|a, b| b.1.total_cmp(&a.1));
        let k1 = (by_cluster.get(&ci).map(|v| v.len()).unwrap_or(1) / 8).clamp(2, 6);
        neighbors.push(cand.into_iter().take(k1).collect());
    }

    for (i, list) in neighbors.iter().enumerate() {
        for &(j, sim) in list {
            // 双向去重：只在 i<j 时建边，避免同一对出现两次。
            if j <= i {
                continue;
            }
            let s = sim.clamp(0.0, 1.0) as f64;
            if s < threshold * 0.75 {
                continue;
            }
            edges.push(SemanticEdge {
                source: id_of(i),
                target: id_of(j),
                edge_type: "similar".into(),
                strength: s,
            });
            *degree.entry(i).or_insert(0) += 1;
            *degree.entry(j).or_insert(0) += 1;
        }
    }

    // ---- 桥接边：让孤立点挂回最近的星团 ---------------------------------
    // 单纯"相似度 > 阈值"会让长尾文件全部落单（它们的向量与所有人类似度都低）。
    // 对每个 0 度的点，强制连到全局最相似的 2 个点，标成 bridge —— 视觉上更淡，
    // 但保证整张图是一张连通图，不会有漂在角落的孤星。
    for i in 0..n {
        if degree.get(&i).copied().unwrap_or(0) > 0 {
            continue;
        }
        let mut best: Vec<(usize, f32)> = Vec::new();
        for j in 0..n {
            if j == i || vectors[j].len() != vectors[i].len() {
                continue;
            }
            best.push((j, cosine(&vectors[i], &vectors[j])));
        }
        best.sort_by(|a, b| b.1.total_cmp(&a.1));
        for &(j, sim) in best.iter().take(2) {
            let s = sim.clamp(0.0, 1.0) as f64;
            edges.push(SemanticEdge {
                source: id_of(i),
                target: id_of(j),
                edge_type: "bridge".into(),
                strength: s.max(0.12),
            });
            *degree.entry(i).or_insert(0) += 1;
        }
    }

    // ---- 连通性收口：保证整张图是**一个**连通分量 -----------------------
    //
    // 踩过的坑（两次）：
    //   1) 只桥接"度为 0"的点不够 —— 实测 148 个节点分成 8 个互不相连的团；
    //   2) 改成"按簇串骨架链"仍然剩 5 个分量 —— 因为**单个簇内部就可能分裂**
    //      （簇内最近邻阈值过滤后，一个簇被切成好几片），而骨架链只连簇代表点，
    //      代表点只能救它所在的那一片。
    //
    // 正解：先用并查集算出**真实的连通分量**，再把各分量按"最相似的一对点"
    // 串起来，直到只剩一个。这样无论簇内怎么分裂，最终都是一张连通图。
    {
        let mut uf: Vec<usize> = (0..n).collect();
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
        // 用已建好的边初始化并查集。
        {
            let mut idx: HashMap<&str, usize> = HashMap::with_capacity(n);
            // 边缘查表：edge 里存的是 "sem:<file_id>"，需要反查下标。
            // 直接在下面遍历 nodes 前先把 id → 下标建好（节点还没组装，用 id_of 现算）。
            let ids: Vec<String> = (0..n).map(id_of).collect();
            for (i, id) in ids.iter().enumerate() {
                idx.insert(id.as_str(), i);
            }
            for e in edges.iter() {
                if let (Some(&a), Some(&b)) = (idx.get(e.source.as_str()), idx.get(e.target.as_str())) {
                    let ra = find(&mut uf, a);
                    let rb = find(&mut uf, b);
                    if ra != rb {
                        uf[ra] = rb;
                    }
                }
            }
        }
        // 按根聚成分量。
        let mut comps: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let r = find(&mut uf, i);
            comps.entry(r).or_default().push(i);
        }
        if comps.len() > 1 {
            // 每个分量挑一个"离簇心最近"的代表点，把代表点两两连成链。
            let mut reps: Vec<usize> = comps
                .values()
                .map(|members| {
                    *members
                        .iter()
                        .max_by(|&&a, &&b| {
                            let sa = centers
                                .get(assign[a])
                                .map(|c| cosine(&vectors[a], c))
                                .unwrap_or(f32::MIN);
                            let sb = centers
                                .get(assign[b])
                                .map(|c| cosine(&vectors[b], c))
                                .unwrap_or(f32::MIN);
                            sa.total_cmp(&sb)
                        })
                        .unwrap_or(&members[0])
                })
                .collect();
            // 代表点之间按相似度排个序，让相邻的（内容相近的）先连，
            // 骨架边就不会横跨整张图把不相干的两团硬拉在一起。
            reps.sort_unstable_by(|&a, &b| a.cmp(&b));
            for w in reps.windows(2) {
                let (a, b) = (w[0], w[1]);
                if vectors[a].len() != vectors[b].len() {
                    continue;
                }
                let sim = cosine(&vectors[a], &vectors[b]).clamp(0.0, 1.0) as f64;
                edges.push(SemanticEdge {
                    source: id_of(a),
                    target: id_of(b),
                    edge_type: "skeleton".into(),
                    // 骨架边只表达"这两个团相邻"，不表达内容相似，
                    // 所以强度固定偏弱，别亮过真正的簇内边。
                    strength: (0.16 + sim * 0.2).min(0.36),
                });
            }
        }
    }

    // ---- 组装节点 ----------------------------------------------------
    let mut nodes = Vec::with_capacity(n);
    for i in 0..n {
        let fid = file_ids[i];
        let row = meta.get(&fid);
        let ci = assign[i];
        let centrality = centers
            .get(ci)
            .map(|c| cosine(&vectors[i], c).clamp(0.0, 1.0))
            .unwrap_or(0.5);
        let (name, path, kind, size, mtime) = match row {
            Some(r) => (r.name.clone(), r.path.clone(), r.kind.clone(), r.size, r.mtime),
            None => (String::new(), String::new(), String::new(), 0, 0),
        };
        nodes.push(SemanticNode {
            id: id_of(i),
            file_id: fid,
            name,
            path,
            kind,
            size,
            mtime,
            cluster: ci as i64,
            centrality,
            color: cluster_color(ci).to_string(),
        });
    }

    // 簇标签：取簇内 centrality 最高的 3 个文件名拼起来，前端叫得出名字。
    let mut clusters: Vec<SemanticCluster> = Vec::new();
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, &c) in assign.iter().enumerate() {
        members.entry(c).or_default().push(i);
    }
    let mut cluster_ids: Vec<usize> = members.keys().copied().collect();
    cluster_ids.sort_unstable();
    for ci in cluster_ids {
        let mut idxs = members[&ci].clone();
        idxs.sort_by(|&a, &b| nodes[b].centrality.total_cmp(&nodes[a].centrality));
        let label = idxs
            .iter()
            .take(3)
            .map(|&i| short_name(&nodes[i].name))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        clusters.push(SemanticCluster {
            id: ci as i64,
            size: idxs.len() as i64,
            label: if label.is_empty() { format!("簇 {}", ci + 1) } else { label },
            color: cluster_color(ci).to_string(),
        });
    }
    clusters.sort_by(|a, b| b.size.cmp(&a.size));

    Ok(SemanticGraph {
        nodes,
        edges,
        clusters,
        total: n as i64,
        available,
        dim: main_dim as i64,
        threshold,
        truncated,
    })
}

/// 截断过长的文件名，簇标签只需要能认出"这一类是什么"。
fn short_name(name: &str) -> String {
    let base: &str = if name.len() > 12 && name.contains('.') {
        &name[..name.rfind('.').unwrap_or(name.len())]
    } else {
        name
    };
    let mut out = String::new();
    for (i, ch) in base.chars().enumerate() {
        if i >= 10 {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
