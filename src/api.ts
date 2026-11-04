// 与 Rust 后端通信的唯一入口。
// 所有命令名必须与 src-tauri/src/cmd.rs 里的 #[tauri::command] 一致。
import { invoke } from "@tauri-apps/api/core";

// ---------------------------------------------------------------- 类型

export interface AppInfo {
  name: string;
  version: string;
  display_version: string;
  provider: string;
  files: number;
}

export interface Hit {
  id: number;
  path: string;
  dir: string;
  name: string;
  ext: string;
  kind: string;
  kind_label: string;
  size: number;
  size_label: string;
  mtime: number;
  mtime_label: string;
  category: string;
  /** 是否有缩略图（文件名固定为 `<id>.jpg`） */
  thumb: boolean;
  thumb_key?: string | null;
  score: number;
  reason: string;
  /** One-based page/slide number when a page-level match is available. */
  page?: number | null;
  snippet: string;
  /** 视觉标签（图片/视频自动生成）：蓝天、草地、雪景、夕阳… */
  tags?: string | null;
}

export interface SearchQuery {
  text: string;
  kinds: string[];
  exts: string[];
  roots: string[];
  categories: string[];
  size_min?: number | null;
  size_max?: number | null;
  mtime_after?: number | null;
  mtime_before?: number | null;
  limit?: number;
  offset?: number;
  sort?: string;
  desc?: boolean;
  use_fts?: boolean;
  use_pinyin?: boolean;
  use_fuzzy?: boolean;
  use_semantic?: boolean;
  /** 用文字描述找图片/声音（需要装了 CLIP/CLAP 模型），三态：不传=跟随界面开关 */
  cross_modal?: boolean | null;
  /** 是否把「工作区目录」里尚未入库的文件也一起检索（正文现场抽取），不传=开启 */
  workspace?: boolean | null;
  /** Home-page scope: all, indexed, or path. */
  scope?: "all" | "indexed" | "path" | string | null;
  scope_path?: string | null;
}

export interface WorkspaceState {
  recent_paths: string[];
  pinned_paths: string[];
  last_path: string;
  last_scope: string;
  startup_scope: string;
  startup_path: string;
  initial_scope: string;
  initial_path: string;
}

export interface StatsOut {
  files: number;
  bytes: number;
  bytes_label: string;
  kinds: { kind: string; label: string; count: number; bytes: number }[];
  text_ok: number;
  embed_ok: number;
  missing: number;
  db_bytes: number;
  vector_count: number;
  dim: number;
}

export interface ScanProgress {
  running: boolean;
  dirs: number;
  files: number;
  added: number;
  updated: number;
  skipped: number;
  errors: number;
  missing: number;
  bytes: number;
  discovered_files: number;
  discovered_bytes: number;
  done: boolean;
  cancelled: boolean;
  current: string;
  seconds: number;
  extensions: { ext: string; kind: string; count: number }[];
  roots: {
    root: string; dirs: number; files: number; added: number; updated: number;
    skipped: number; errors: number; missing: number; bytes: number;
    discovered_files: number; discovered_bytes: number; last_error: string;
    extensions: { ext: string; kind: string; count: number }[];
  }[];
}

export interface IndexStats {
  queued: number;
  total: number;
  completed: number;
  cancelled: boolean;
  running: boolean;
  text_done: number;
  text_fail: number;
  emb_done: number;
  emb_fail: number;
  thumb_done: number;
  current: string;
  current_items: string[];
  /** Backend task elapsed time, in whole seconds. */
  elapsed_seconds: number;
}

export interface ModelInfo {
  backend: string;
  model: string;
  dim: number;
  device: string;
  modalities: string[];
  ready: boolean;
  detail: string;
  /** 跨模态图片模型（中文 CLIP）状态，空串 = 未安装 */
  clip: string;
  /** 跨模态音频模型（CLAP）状态，空串 = 未安装 */
  clap: string;
}

export interface GpuDevice {
  id: string;
  name: string;
  backend: string;
  total_memory: number;
  free_memory: number;
  integrated: boolean;
}

/** 编译期链入的一个 GGML 后端（CUDA / Vulkan / Metal / CPU）。 */
export interface GpuBackendInfo {
  name: string;
  accelerator: boolean;
  device_count: number;
  features: [string, string][];
}

/** 单个加速设备的自描述，显存按 MiB 计。 */
export interface GpuDeviceInfo {
  id: string;
  name: string;
  backend: string;
  vendor: string;
  total_mb: number;
  free_mb: number;
  integrated: boolean;
  /** 显存分档 0~3：0=≥8GiB，1=4~8GiB，2=1~4GiB，3=<1GiB（等同 CPU）。 */
  tier: number;
}

/**
 * GPU 能力报告（REQ-093）。字段刻意贴近需求表的
 * `{backend, gpu_name, vram_mb, cuda_version, status}` 形状。
 */
export interface GpuReport {
  generated_at: string;
  platform: string;
  arch: string;
  backend: string;
  gpu_name: string;
  vendor: string;
  vram_mb: number;
  vram_free_mb: number;
  cuda_version: string | null;
  /** 当前安装包是否编入了 CUDA 后端（决定要不要校验 nvcc）。 */
  cuda_required: boolean;
  driver_version: string | null;
  /** ready | degraded | cpu_only */
  status: string;
  tier: number;
  reason: string;
  backends: GpuBackendInfo[];
  devices: GpuDeviceInfo[];
  notes: string[];
  /** 按显卡型号给出的 CUDA Toolkit 版本建议，未启用 CUDA 后端时为 null。 */
  cuda_hint: string | null;
  /** 是否装过可选的 ONNX Runtime 跨模态扩展。 */
  ort_present: boolean;
  /** 装了扩展时按此顺序协商 ExecutionProvider。 */
  ort_providers: string[];
}

/** 模型目录里的一个 GGUF 文件 */
export interface ModelEntry {
  path: string;
  /** 展示名：GGUF 头里的 general.name，读不到就是文件名 */
  name: string;
  file: string;
  size: number;
  size_label: string;
  mtime: number;
  mtime_label: string;
  /** embedding | chat | projector | unknown */
  kind: string;
  kind_label: string;
  /** 是否当前正在使用 */
  active: boolean;
  /** gguf 或 safetensors */
  format: string;
  /** Required model config, tokenizer and weights are present. */
  files_complete: boolean;
  /** 当前构建是否已包含可直接加载此模型的原生后端 */
  direct_loadable: boolean;
  /** 模型文件所在目录 */
  model_dir: string;
  /** 模型声明支持的输入模态：text / image / audio / video */
  modalities: string[];
  /** 可用的推理适配器候选；不代表扩展已安装 */
  candidate_backends: string[];
}

export interface ModelImportFile {
  path: string;
  relative_path: string;
  format: string;
  kind: string;
  kind_label: string;
  size: number;
}

export interface ModelImportResult {
  source: string;
  destination: string;
  copied_files: number;
  total_bytes: number;
  files: ModelImportFile[];
  primary_model: string;
  primary_kind: string;
  files_complete: boolean;
  direct_loadable: boolean;
  needs_conversion: boolean;
  warnings: string[];
  architecture: string;
  components: string[];
  /** 导入模型声明支持的输入模态：text / image / audio / video */
  modalities: string[];
  /** 可用的推理适配器候选；不代表扩展已安装 */
  candidate_backends: string[];
}

export interface TreeNode {
  name: string;
  path: string;
  size: number;
  count: number;
  kinds: Record<string, number>;
  children: TreeNode[];
}

/** 右上角「索引目录结构」面板的一个节点（目录或文件）。 */
export interface TreeEntry {
  name: string;
  path: string;
  is_file: boolean;
  ext: string;
  kind: string;
  size: number;
  mtime: number;
  /** 直属文件数（目录）；文件为 1 */
  files: number;
  /** 直属子目录数（目录） */
  dirs: number;
  /** 递归文件数（目录） */
  total_files: number;
  /** 递归字节数（目录） */
  total_bytes: number;
  /** 已索引条目数（目录按整目录算） */
  indexed: number;
  /** 待索引条目数（目录按整目录算） */
  pending: number;
  error: string;
}

/** 面板里「某一层」的展开结果。 */
export interface TreeLevel {
  path: string;
  name: string;
  entries: TreeEntry[];
  files: number;
  dirs: number;
  total_files: number;
  total_bytes: number;
  indexed: number;
  pending: number;
  hidden_files: number;
}

/** 星系图谱骨架（cmd::graph_data）—— 需求 REQ-042~052 的最小落地版本。 */
export interface GraphNodeOut {
  id: string;
  node_type: string;
  name: string;
  path: string;
  kind: string;
  size: number;
  count: number;
  depth: number;
  mtime: number;
}

export interface GraphEdgeOut {
  source: string;
  target: string;
  edge_type: string;
  strength: number;
}

export interface GraphDataOut {
  root: string;
  name: string;
  nodes: GraphNodeOut[];
  edges: GraphEdgeOut[];
  total_files: number;
  total_bytes: number;
  truncated: number;
}

/** 星图节点（star 模块，对应 Rust 侧 StarNode）。 */
export interface StarNodeOut {
  id: string;
  name: string;
  node_type: string;
  path: string;
  kind: string;
  size: number;
  count: number;
  depth: number;
  mtime: number;
  parent: string | null;
  has_children: boolean;
  cluster: number;
  color: string;
  /** 恒星光谱型（仅盘符节点）：system / data / archive / removable。 */
  star_class?: string;
  /** 物理容量（字节），仅盘符节点。0 表示取不到。 */
  capacity?: number;
  /** 已用比例 0~1，画恒星外圈进度光环。 */
  used_ratio?: number;
}

/** 盘符物理信息（star_drives）：恒星大小与引力的数据源。 */
export interface DriveOut {
  mount: string;
  letter: string;
  label: string;
  fs: string;
  total_bytes: number;
  free_bytes: number;
  removable: boolean;
  star_class: string;
  /** 引擎直接用的世界坐标半径。 */
  star_radius: number;
  /** 引力强度 0~1。 */
  gravity: number;
}

/** 语义星图节点（向量聚类的一个文件）。 */
export interface SemanticNodeOut {
  id: string;
  file_id: number;
  name: string;
  path: string;
  kind: string;
  size: number;
  mtime: number;
  cluster: number;
  centrality: number;
  color: string;
}

export interface SemanticEdgeOut {
  source: string;
  target: string;
  edge_type: string;
  strength: number;
}

export interface SemanticClusterOut {
  id: number;
  size: number;
  label: string;
  color: string;
}

export interface SemanticGraphOut {
  nodes: SemanticNodeOut[];
  edges: SemanticEdgeOut[];
  clusters: SemanticClusterOut[];
  total: number;
  available: number;
  dim: number;
  threshold: number;
  truncated: boolean;
}

export interface StarEdgeOut {
  source: string;
  target: string;
  edge_type: string;
  strength: number;
}

export interface StarGraphOut {
  root: string;
  name: string;
  level: number;
  nodes: StarNodeOut[];
  edges: StarEdgeOut[];
  total_files: number;
  total_bytes: number;
  hidden_files: number;
  hidden_bytes: number;
  truncated: number;
}

/** 思维模型图的条目（REQ-053~061 八种模型共用）。 */
export interface MindItem {
  name: string;
  path: string;
  kind: string;
  ext: string;
  size: number;
  mtime: number;
  dir: string;
}

/** 一级分组：颜色、计数、条目列表。 */
export interface MindGroup {
  key: string;
  label: string;
  count: number;
  bytes: number;
  color: string;
  items: MindItem[];
}

/** 八种思维模型图的数据（cmd::mind_map_data）。 */
export interface MindMapOut {
  model: string;
  title: string;
  subtitle: string;
  groups: MindGroup[];
  total_files: number;
  total_bytes: number;
  /** 时间线模型用：按时间桶排好的分组顺序。 */
  buckets?: string[];
}

export interface WatchStatus {
  running: boolean;
  roots: number;
  events: number;
}

export interface ServerStatus {
  running: boolean;
  url: string;
}

export interface ExtensionStatus {
  onnxruntime: boolean;
  onnxruntime_path: string;
  onnxruntime_version: string;
  detail: string;
  python: boolean;
  python_path: string;
  root: string;
}

export interface Activity {
  id: number;
  ts: number;
  kind: string;
  msg: string;
}

export interface Rule {
  id: number;
  name: string;
  field: string;
  op: string;
  value: string;
  category: string;
  priority: number;
  enabled: boolean;
}

export interface AppConfig {
  scan: any;
  index: any;
  model: any;
  search: any;
  watch: any;
  ui: any;
  workspace: any;
  server: any;
}

export interface RootIndexStats {
  root: string;
  enabled: boolean;
  last_scan: number;
  index_progress: number;
  kinds_label: string;
  files: number;
  bytes: number;
  bytes_label: string;
  added: number;
  updated: number;
  skipped: number;
  errors: number;
  missing: number;
  text_ok: number;
  embed_ok: number;
  completed: number;
  text_pending: number;
  embed_pending: number;
  last_error: string;
  kinds: { kind: string; label: string; count: number; bytes: number }[];
  discovered_files: number;
  discovered_bytes: number;
  extensions?: { ext: string; kind: string; count: number }[];
}

/** 部分配置（深合并用）：后端会把传入的字段并入当前配置，其余保持不变。 */
export type PartialConfig = DeepPartial<AppConfig>;
export type DeepPartial<T> = T extends object
  ? { [K in keyof T]?: DeepPartial<T[K]> }
  : T;

// ---------------------------------------------------------------- 调用

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  getConfig: () => invoke<AppConfig>("get_config"),
  saveConfig: (cfg: PartialConfig) => invoke<void>("save_config", { cfg }),

  search: (q: SearchQuery) => invoke<Hit[]>("search", { q }),
  searchSemantic: (q: SearchQuery) => invoke<Hit[]>("search_semantic", { q }),
  similar: (fileId: number, kind?: string, top?: number) =>
    invoke<Hit[]>("similar", { fileId, kind, top }),
  stats: () => invoke<StatsOut>("stats"),
  recent: (limit = 50) => invoke<Hit[]>("recent", { limit }),

  scanStart: (roots?: string[], indexWhat?: string, scanOnly = false) =>
    invoke<void>("scan_start", { roots, indexWhat, scanOnly }),
  scanCancel: () => invoke<void>("scan_cancel"),
  scanProgress: () => invoke<ScanProgress>("scan_progress"),
  indexRoots: () => invoke<RootIndexStats[]>("index_roots"),

  indexStart: (what?: string, force?: boolean, roots?: string[]) =>
    invoke<number>("index_start", { what, force, roots }),
  indexCancel: () => invoke<void>("index_cancel"),
  indexStats: () => invoke<IndexStats>("index_stats"),

  modelInfo: () => invoke<ModelInfo>("model_info"),
  gpuDevices: () => invoke<GpuDevice[]>("gpu_devices"),
  /** 完整 GPU 能力报告（REQ-093），设置页「显示适配」用它。 */
  gpuReport: () => invoke<GpuReport>("gpu_report"),
  /** 重新探测并落盘 gpu_report.log，返回写入路径。 */
  gpuRefreshReport: () => invoke<string>("gpu_refresh_report"),
  modelLoad: (backend: string, path?: string) =>
    invoke<ModelInfo>("model_load", { backend, path }),
  modelDeactivate: () => invoke<ModelInfo>("model_deactivate"),
  modelList: () => invoke<ModelEntry[]>("model_list"),
  modelDir: () => invoke<string>("model_dir_path"),
  modelDelete: (path: string) => invoke<void>("model_delete", { path }),
  modelImport: (src: string) => invoke<string>("model_import", { src }),
  modelImportFolder: (src: string) => invoke<ModelImportResult>("model_import_folder", { src }),

  openPath: (path: string) => invoke<void>("open_path", { path }),
  openIndexedPath: (fileId: number, path: string) =>
    invoke<void>("open_indexed_path", { fileId, path }),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  renamePath: (path: string, name: string) => invoke<string>("rename_path", { path, name }),
  ensurePreview: (fileId: number, path: string) =>
    invoke<boolean>("ensure_preview", { fileId, path }),
  savePdfThumbnail: (fileId: number, path: string, dataUrl: string) =>
    invoke<void>("save_pdf_thumbnail", { fileId, path, dataUrl }),

  opExport: (paths: string[], out: string, fmt?: string) =>
    invoke<number>("op_export", { paths, out, fmt }),

  rulesList: () => invoke<Rule[]>("rules_list"),
  rulesSave: (rules: Rule[]) => invoke<void>("rules_save", { rules }),
  rulesApply: () => invoke<number>("rules_apply"),

  activityList: (kind?: string, limit = 300) =>
    invoke<Activity[]>("activity_list", { kind, limit }),

  treeData: (root?: string, depth = 3) =>
    invoke<TreeNode>("tree_data", { root, depth }),

  /** 索引目录结构面板：不传 path 取索引根层，传 path 展开该目录。 */
  treeBrowse: (path?: string, fileLimit = 200) =>
    invoke<TreeLevel>("tree_browse", { path, fileLimit }),
  treeSearch: (query: string, limit = 200) =>
    invoke<TreeEntry[]>("tree_search", { query, limit }),

  /** 星系图谱骨架：根 + depth 层目录 + 每目录前 N 个文件。 */
  graphData: (root?: string, depth = 2, maxNodes = 900, fileLimit = 31) =>
    invoke<GraphDataOut>("graph_data", { root, depth, maxNodes, fileLimit }),

  /** 盘符物理信息：容量 / 可用 / 卷标 / 恒星半径与引力。 */
  starDrives: () => invoke<DriveOut[]>("star_drives"),
  /** 语义星图：按向量相似度聚类建边（"排序模式"的核心）。 */
  starSemantic: (maxNodes = 260, threshold = 0.55) =>
    invoke<SemanticGraphOut>("star_semantic", { maxNodes, threshold }),
  /** 星图渐进式探索：Level 0 全局概览（类型节点 + 根，需求 #19）。 */
  starOverview: () => invoke<StarGraphOut>("star_overview"),
  /** 星图：展开某个节点的下一层（需求 #20/#21）。 */
  starChildren: (nodeId?: string, fileLimit = 60, dirLimit = 80) =>
    invoke<StarGraphOut>("star_children", { nodeId, fileLimit, dirLimit }),
  /** 星图布局缓存（需求 #52 的 Rust 侧一级，TTL 5 分钟）。 */
  starLayoutGet: (key: string) => invoke<number[][] | null>("star_layout_get", { key }),
  starLayoutPut: (key: string, positions: number[]) =>
    invoke<boolean>("star_layout_put", { key, positions }),

  /** 思维模型图：把一组文件按八种模型之一整理成图数据。 */
  mindMapData: (model: string, paths: string[], query?: string) =>
    invoke<MindMapOut>("mind_map_data", { model, paths, query }),
  workspaceAdd: (dir: string) => invoke<string[]>("workspace_add", { dir }),
  workspaceState: () => invoke<WorkspaceState>("workspace_state"),
  workspaceRemember: (path: string, scope?: string) =>
    invoke<WorkspaceState>("workspace_remember", { path, scope }),
  workspaceSetActive: (scope: string, path?: string) =>
    invoke<WorkspaceState>("workspace_set_active", { scope, path }),
  workspaceRemove: (path: string) => invoke<WorkspaceState>("workspace_remove", { path }),
  workspaceSetStartup: (scope: string, path?: string) =>
    invoke<WorkspaceState>("workspace_set_startup", { scope, path }),
  availableDrives: () => invoke<string[]>("available_drives"),

  watchStart: () => invoke<number>("watch_start"),
  watchStop: () => invoke<void>("watch_stop"),
  watchStatus: () => invoke<WatchStatus>("watch_status"),

  serverStart: () => invoke<string>("server_start"),
  serverStop: () => invoke<void>("server_stop"),
  serverStatus: () => invoke<ServerStatus>("server_status"),

  dbVacuum: () => invoke<void>("db_vacuum"),
  dbClear: () => invoke<void>("db_clear"),
  dataDir: () => invoke<string>("data_dir_path"),
  extensionStatus: () => invoke<ExtensionStatus>("extension_status"),
  extensionInstall: (kind: "onnxruntime" | "python") =>
    invoke<ExtensionStatus>("extension_install", { kind }),
  extensionRemove: (kind: "onnxruntime") =>
    invoke<ExtensionStatus>("extension_remove", { kind }),
  extensionUpdate: () => invoke<ExtensionStatus>("extension_update", { kind: "onnxruntime" }),
};

/** Tauri 未注入时（比如直接用浏览器打开 vite dev）给一个明确的错误。 */
export function isTauri(): boolean {
  return typeof (window as any).__TAURI_INTERNALS__ !== "undefined";
}
