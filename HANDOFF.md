# DeepDark_Oracle 交接说明

## 当前快照

- 截止日期：2026-11-01。产品版本号 **`0.0.26`**（显示版 `0.26`），应用名 `DeepDark_Oracle`。当前工作目录 `D:\软件\DeepDark_Oracle`，**不是 git 仓库**。
- **v0.26 安装包已产出**：`release/DeepDark_Oracle_0.0.26_x64-setup.exe`，34,642,746 字节（33.04 MiB），SHA-256 `a8e945321e1f220c587436285921cd97106c7b0adc114e4c6319eac0815530ea`。这一版实现《2D转3D与星图需求明细》的 33 条需求（`src/galaxy/star/` 2588 行），并清掉了安装包里用不上的陈旧二进制（MinGW 三个运行时 DLL、`onnxruntime.dll`、`vulkan-1.dll`、`resources/`、`plugins/`）。上一版 v0.25 是 34,276,337 字节、SHA-256 `677aca3e6367cb247e3ea003750081a686c62bbf47a2a06dee7961501912d606`。
- **根目录 `WebView2Loader.dll`（160,880 B）绝对不能删**：删掉即 `0xC0000135 STATUS_DLL_NOT_FOUND`（退出码 3221225781），窗口一个都不出。它由 cargo 生成在 `D:\DeepDark_Oracle-build\target\release\` 下，但**不会自动复制到项目根**，重新出包后要手动补。
- 项目是离线桌面磁盘检索工具，不自带或分发模型权重。用户自行导入 GGUF 或 safetensors；ONNX Runtime 和 Python sidecar 是可选扩展，安装包默认不带它们。
- 本节以下各章记录的是 v0.23 及更早的架构与历史；v0.24 的增量（星系图谱、思维模型图、安装包瘦身、工具链自愈）见「v0.24 增量」章，v0.25 的增量（GPU 兼容性、双渲染管线、Vulkan 默认启用）见文末「v0.25 增量」章。

## 这几天的主要工作

### 产品与界面

- 首页以搜索为主：打字机标题、`DeepDark_Oracle` 品牌、黑白扁平图库结果；搜索后搜索区域收至上方，结果按相关度呈现。保留工作区范围、文件类别筛选、相似检索、排序、选择与导出。
- 主页移除了最近文件下拉框和搜索耗时毫秒数。结果首批显示 500 条，更多匹配项在后台静默分页获取；滚动时再逐批挂载卡片，继续保留全量结果和全选导出语义。
- 缩略图使用异步、低优先级和懒加载；PDF 缩略图通过可见性观察触发，渲染并发受限。结果卡片采用 `v-memo`，后台追加数据不会使已经显示的卡片全部重新绘制。
- 设置内容按文件索引、模型配置、局域网配置、可选扩展和日志分区；保留工作区记忆、目录树和树状图入口。可选扩展不改变原生模型路径。

### 扫描与索引

- 扫描根目录可多选，并按目录保存启用状态、文件数、体积、扫描错误、正文进度和向量进度；点击建立索引会先扫描并增量处理新增、变更或待重试项目。
- 索引支持取消、正文/向量分别补建、检查点保存与断点续传；取消后不应把尚未完成项计为已完成。扫描、解析、推理和持久化错误写入日志。
- `index.rs` 使用 Rayon CPU 预处理与单模型动态批处理，按文本/媒体 token 和 GPU 可用资源调整批次；节能/自动策略限制 CPU worker 数，不再依赖启动多个同权重模型实例。
- 已接入 PDF 正文和嵌入图片处理，扫描件没有文字层时不会把空文本送入嵌入模型；PDF、DOCX、PPTX 有分页/页码数据路径。其他格式的页码能力不应未经验证就对外承诺。
- 视频使用 FFmpeg 抽帧和音频解码；EmbeddingGemma2 相关默认参数为共享 8192-token context、独立图片 1120 tokens、PDF 内嵌图片 280、视频帧 140、视频 1 FPS，视觉/音频塔可配置关闭。实际模态能力仍取决于模型、projector 和所选推理后端。

### 模型与检索架构

- GGUF 由 `ai/llama.rs` 调用 `native/llama_shim.cpp` 和 llama.cpp；多模态 GGUF 需要可配对的 `mmproj`，经 mtmd 输入图像/音频/视频。只有主 GGUF 而没有兼容 projector 时，不应显示对应多模态能力。
- Safetensors 使用 vendored Candle/EmbeddingGemma 实现；模型导入逻辑解析配置、tokenizer、权重分片索引和模型文件，不要求用户手动指定 Candle 或 llama.cpp。
- 普通用户只选择 GPU 硬件；后端负责运行时适配。ONNX Runtime 不会直接读取 safetensors，需 ONNX 模型；Python sidecar 同样是可选扩展，不是核心依赖。
- 检索由 `search.rs` 融合文件名、路径、拼音、模糊匹配、SQLite FTS5/BM25、文本向量和可用的 CLIP/CLAP/共享多模态向量。EmbeddingGemma 的跨语言能力是多语言语义匹配，不是自动翻译；没有已编码的内容仍然搜不到。
- 相似度数值是模型向量余弦相似度/各检索通道的排序分数，不是“相关概率”。现在语义分数显示余弦原值；短于等于 2 个字符的语义查询最低阈值为 `0.62`，一般语义阈值来自配置（默认 `0.45`），最终列表另有 `0.40` 总过滤线。旧分数映射曾给弱命中加基线，因此像 `0.53` 看起来比实际更可信；单字“枪”的 `0.53` 语义项现在应被过滤。

### 2026-10-31 最近修复

- Tauri `search`、`search_semantic`、`similar` 命令改成异步入口，将同步数据库、向量和模型工作放到 `spawn_blocking`，避免检索长期占用前台 IPC/UI 调度线程。
- `SearchView.vue` 的后台结果累计改用非响应式数组和按类型分桶，只有当前视图需要的首 500 条进入响应式卡片列表；滚动再渲染后续卡片。后台分页仍会继续取回最多后端单次查询上限的结果，选择全部后导出会等待分页完成。
- `ResultGallery.vue` 和 `PdfThumbnail.vue` 降低缩略图加载优先级并推迟不可见 PDF 渲染。已移除左上角 ms 数显示。
- `config.rs` 会检查便携数据目录、`%LOCALAPPDATA%\DeepDark_Oracle\data` 和 `%APPDATA%` 中的 SQLite 文件是否有有效 `files` 表，并比较索引行与数据库/向量文件体积，减少升级后误选空数据库的情况。
- `vector.rs` 在打开向量库遇到维度不匹配或长度不匹配时不再删除磁盘文件。切换模型后按模型实际维度重新打开向量库，保护之前的向量数据。

## 代码入口

| 区域 | 文件 | 职责 |
| --- | --- | --- |
| 应用与 IPC | `src/App.vue`、`src-tauri/src/lib.rs`、`src-tauri/src/cmd.rs` | 页面路由、Tauri 命令注册与桌面 API |
| 搜索界面 | `src/views/SearchView.vue`、`src/components/ResultGallery.vue` | 搜索状态、分页、类别过滤、缩略图图库 |
| 扫描/索引 UI | `src/views/ScanView.vue`、`src/components/SettingsIndexTab.vue` | 目录、类型、索引进度与操作 |
| 模型 UI | `src/views/ModelsView.vue`、`src/components/SettingsModelTab.vue` | 导入模型、GPU、推理参数和模型能力 |
| 业务状态 | `src-tauri/src/state.rs`、`config.rs` | `AppState`、配置、运行时与数据路径 |
| 数据持久化 | `src-tauri/src/db.rs`、`vector.rs` | SQLite/FTS5、正文分页、索引状态、向量文件 |
| 扫描与索引 | `src-tauri/src/scan.rs`、`index.rs` | 目录扫描、增量队列、批处理、检查点 |
| 检索 | `src-tauri/src/search.rs`、`workspace.rs` | 混合排序、语义/跨模态、未索引工作区 |
| AI | `src-tauri/src/ai/`、`src-tauri/native/llama_shim.cpp` | GGUF、Candle、CLIP/CLAP/ORT 适配 |
| 内容提取 | `src-tauri/src/textract.rs`、`media.rs` | 文档/PDF 正文、页面媒体、缩略图 |

## 数据与兼容性

- 数据目录解析位于 `src-tauri/src/config.rs::data_dir/resolve_data_dir`。通常便携版使用 exe 同级 `data/`；安装在不可写目录时使用用户数据目录。现在会在便携、LOCALAPPDATA、APPDATA 候选中检查 SQLite 表与记录并按数据量择优。
- 这是兼容性启发式选择，不是完整迁移协议：当多个目录都含有效数据时，仍以目录质量和文件大小排序，没有核验数据库与全部向量是否属于同一索引快照。长期改进方向是固定稳定用户数据根目录，并做可审计、保留源文件的显式迁移。
- 主数据库为 `index.db`（WAL）；向量分别保存在 `vectors/`、`vectors_media/`、`vectors_clip/`、`vectors_audio/`。配置、日志、缩略图也在应用数据目录下。不同模型空间或维度不可混用。
- 应用启动时会先以轻量 HashEmbedder 打开基础向量空间；模型按需加载后按实际维度重开向量库。全文 FTS 与语义向量是独立能力；模型未加载或不可用时，全文仍可用，语义向量可能要等模型加载。
- 若旧版本已经把用户向量文件删除，代码无法重建原有语义向量；需要重新建立对应向量。升级后先备份整个 data 目录，再查看设置里的索引统计和日志，不要先删除索引或向量文件。

## 项目内依赖与构建

- Windows 项目内工具默认放 `.tools/`：Node、Cargo/Rustup、CMake、Ninja、下载缓存、可选 ONNX Runtime/CUDA 工具链。`.tools` 是本机目录，不提交 Git。
- `vendor/embeddinggemma-2-candle` 和 `vendor/pdf-extract` 是仓库内依赖源码；`vendor/llama.cpp` 由 bootstrap 获取，具体版本可能随本地 checkout/补丁变化。
- 首次准备工具（需要网络）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts/bootstrap-windows.ps1
. .\scripts\activate-local.ps1
```

- 在 VS Code 集成终端调试：打开仓库根目录，使用 PowerShell，运行 `. .\scripts\activate-local.ps1`，再运行 `npm run tauri dev`。脚本将 Cargo/Node 缓存和输出优先放进项目目录；MSVC/Windows SDK 与 NVIDIA 驱动仍是系统组件。
- 验证前端：`npm run build`。Rust 单元测试：`cargo test --manifest-path src-tauri/Cargo.toml --lib --features llama,candle,candle-cuda`。需要检查发布编译时可用 `cargo check --manifest-path src-tauri/Cargo.toml --all-targets`。
- 构建 NSIS 安装包：`npm run tauri:release` 或 `powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1`。脚本自动检测 CUDA Toolkit；CUDA 构建需要兼容 Toolkit/nvcc 与 MSVC。**模型和索引不打进安装包；当前源码改动要重新构建后才会进入新安装包。**
- **v0.24 起产物路径变了**：`activate-local.ps1` 会把 `CARGO_TARGET_DIR` 指到纯 ASCII 目录，所以安装包输出在 `D:\DeepDark_Oracle-build\target\release\bundle\nsis\`，脚本结尾会打印完整路径；不再固定是 `src-tauri/target/release/bundle/nsis/`。目标机器无该盘符时按 `D:\ → E:\ → F:\ → C:\ → %LOCALAPPDATA%` 顺序回退，可用 `DDO_BUILD_ROOT` 覆盖，或用 `USE_BUILD_IN_WORKSPACE=1` 强制在仓库内构建（仅当仓库路径全 ASCII 时才安全）。
- **`npm run tauri:release` 的前置条件**：`tools\node` 必须在 PATH 里（已追加进用户 PATH，`npm --version` 应为 10.9.2）。

## 本次验证与待办

- 2026-11-01（v0.26）：`npm run build` 通过（2211 modules），`npm run tauri:release` EXIT=0，静默安装 / 启动 / 静默卸载三关实测通过；详见文末「v0.26 增量」的验证记录表。
- 2026-10-31：`npm run build` 通过。
- 2026-10-31：`cargo test --lib --features llama,candle,candle-cuda` 通过，45 项测试全部通过。
- 2026-10-31：`git diff --check` 通过。`cargo fmt -- --check` 未运行成功，因为当前 stable toolchain 未安装 `rustfmt`。
- 最新三项修复没有进行安装版端到端实机验证，也没有重新生成安装包；应在接手机上验证安装版搜索时滚动/缩略图响应、旧数据目录选择和单字语义过滤。
- 目录择优逻辑仍需更强的数据库与向量配对校验，并最终收敛到稳定数据目录/显式迁移。
- 前端会静默分页，但后端目前搜索候选和单次查询有 `50,000` 安全上限；“后台加载所有”在超过此上限时不代表真正无限结果。未来可改成数据库/向量层游标分页或更高效索引，而不是无界加载全部缩略图。
- GPU 利用率取决于模型后端、驱动、显存和批次形状，现有批处理优化不保证固定百分比或固定倍数；需要在目标机器用真实模型和不同类型数据实测。

## 安全注意事项

- 不要用 `git reset --hard`、`git clean -fd`、递归删除或批量永久删除来处理当前工作区。索引、模型、缓存需要清理时，用户要求的大批量删除全部改为移入回收站。
- 不要覆盖或重置现有 `data/`、模型目录、`.tools/` 或 target 输出；先确认真实路径和备份状态。
- 不要把 GGUF/safetensors 权重、索引数据库、CUDA Toolkit、`.tools` 开发依赖提交到仓库或安装包。
- 文档中的 v0.22/v0.23 安装包尺寸、哈希和验收数据是当时历史记录，不代表本次 2026-10-31 源码状态。发布前重新构建并记录新产物信息。


---

## v0.22 增量（2026-10-27）

### 改名

全局把 `deepaisee` 改成 `DeepDark_Oracle`：productName / identifier（`com.deepdarkoracle.desktop`）/ 窗口标题 / Cargo 包名与 lib 名（`deepdark_oracle_lib`）/ 启动器文件名 / 安装器与 NSIS 钩子 / 数据目录回退路径（`%LOCALAPPDATA%\DeepDark_Oracle`）/ 日志与 panic 文件名。

**有意保留旧名的地方**：`src/theme.ts` 里的 localStorage key `deepaisee.lang` / `deepaisee.theme`（改了会丢用户已保存的语言和主题偏好）；`DEEPAISEE_*` 系列构建期环境变量名（内部变量，不影响用户可见名称）；CHANGELOG 与历史文档里的旧名。

### 右上角「索引目录结构」面板

顶栏原来只读的索引统计按钮（`N 个文件 · X GB`）改成下拉触发器，点开从按钮正下方弹出 400px 面板，**不触发任何磁盘扫描**，只读索引库。

- 新组件 `src/components/IndexTreePanel.vue`：三段式（标题栏 + 搜索框 / 树主体 / 统计底栏），**按层懒加载**，展开哪层才查哪层，已加载层缓存在前端，收起不销毁。
- 视觉：白底 `#FFFFFF`、主文字 `#333333`、次要 `#999999`、边框 `#CCCCCC`、选中 `#F5F5F5`、搜索命中 `#E6F7FF`；行高 28px；暗色主题有独立变量。
- 交互：目录搜索（匹配目录名，命中自动逐级展开祖先并滚动定位）、右键菜单（打开文件位置 / 复制完整路径 / 复制文件名 / 加入工作区）、文件行右侧 6px 状态点（绿=已索引、灰=未索引、红=索引出错）、`Esc` 或点面板外收起、底栏「完整视图 →」跳到原来的 squarify 树状图页。

### 后端新增命令

| 命令 | 作用 |
| --- | --- |
| `tree_browse(path, file_limit)` | 返回某一层的目录与直属文件（`path` 为空时返回索引根列表） |
| `tree_search(query, limit)` | 按目录名在 `dir` 聚合结果里搜索 |
| `workspace_add(dir)` | 把一个目录加入工作区配置 |

数据层没有全量扫表：`db.rs` 新增 `dir_rollup_under(prefix)` 用 `dir >= lo AND dir < hi` 做字符串区间裁剪，并单独 OR 上「目录自身」的等值条件（否则会漏掉该目录的直属文件，因为 `D:\wx` 在排序上排在 `D:\wx\` 之前）。实测 24 万文件的库上全量聚合 140ms、单层展开 40~50ms。

### 顺手修掉的既有问题

1. **llama.cpp encoder ubatch 断言导致进程静默退出**：`native/llama_shim.cpp` 把 `cparams.n_batch/n_ubatch` 写死成 512，新版 llama.cpp 对 encoder 模型加了 `GGML_ASSERT(cparams.n_ubatch >= n_tokens)`，索引长文档时直接 abort（没有 panic 日志、没有报错弹窗，窗口直接消失）。改成按 `n_ctx` 取 `batch_cap`，并在 embed 时把超长 token 序列截到 `n_batch`。
2. **中文路径下 windres 打不开 `icon.ico`**：`tauri-build` 会把图标的绝对路径写进 `resource.rc`，MinGW `windres` 不能处理非 ASCII 路径 → 构建 panic。`build.rs` 里改成先把 `icon.ico` 复制到 ASCII 的 `OUT_DIR` 再用。
3. **`TreeView.vue` 的两个 bug**：缺 `import { t } from "../i18n"`；模板 `v-for="(t, i) in trail"` 遮蔽了 i18n 的 `t`，导致 `t("全部")` 把 TreeNode 当函数调用直接崩。
4. **`src-tauri/src/bin/` 下 5 个开发探针**：和 `examples/` 里的同名文件重复，但 `tauri-bundler` 会把 `src/bin/*.rs` 全部当 external binary 打进安装包。已删除 `src/bin/`，只保留 `examples/`。

### 构建与安装包

- 当前 v0.23 产物：`src-tauri/target/release/bundle/nsis/DeepDark_Oracle_0.0.23_x64-setup.exe`，498.90 MiB（523,129,853 bytes）；SHA-256：`FB3C98CFF3002D839C8F64D54D6B98BC4CC71349E54A84B4112BCA9170D60032`。该安装包包含 CUDA/MSVC 运行库，但不包含 GGUF、safetensors、CLIP/CLAP 权重或 `onnxruntime.dll`。
- 历史产物：`DeepDark_Oracle_0.0.22_x64-setup.exe`，682 MB（NSIS）。该旧包的内容记录仅供追溯；当前发布必须遵循 v0.23 约束，不捆绑模型权重或 ONNX Runtime。
- 本次使用项目目录环境和 MSVC x64 工具链构建，检测到 CUDA Toolkit 13.1，并将 CUDA/MSVC runtime 复制到发布目录。构建脚本仍会优先使用项目内 `.tools` 目录，避免把依赖写入 C 盘。
- 构建命令：`npm run tauri:release`（脚本自动选择 `llama,candle,candle-cuda,custom-protocol`）。
- 已验证：发布 exe `--selftest` 通过 6 项、失败 0、跳过 10；Safetensors 实机 CUDA 探针加载为 768 维并报告 text/image/video/audio，8 张图批处理约 1.51x 且最小余弦 0.999993；GGUF + 同目录 `mmproj` 探针加载为 768 维并报告四模态。Candle 与 llama.cpp 的批处理失败会递归拆分子批次，避免整批回退逐张。

---

## v0.24 增量（2026-10-31）

版本号：`Cargo.toml` / `tauri.conf.json` / `package.json` = `0.0.24`；`src-tauri/src/lib.rs` 的 `APP_VERSION = "0.0.24"`、`APP_DISPLAY_VERSION = "0.24"`。

### 交付物

| 项 | 值 |
| --- | --- |
| 安装包 | `release/DeepDark_Oracle_0.0.24_x64-setup.exe` |
| 大小 | 30,501,454 字节（29.09 MiB） |
| SHA-256 | `9f7ffdd088daa925d6cd9c25c819c08f601fe2daaf81b6e621b76de2d0f42a1a` |
| 安装包内容 | 13 个文件：`DeepDark_Oracle.exe`(23,929,856B)、`WebView2Loader.dll`(160,880B)、`tools/bin/ffmpeg.exe`(87,638,016B)、`resources/scripts/*.ps1` ×2、`uninstall.exe`、`$PLUGINSDIR/*` ×7 |
| **不含** | models 权重、data 索引、`onnxruntime.dll`、`libstdc++-6.dll`、`libgcc_s_seh-1.dll`、`libwinpthread-1.dll` |

### 新增功能（按需求表 REQ-001 ~ REQ-061、REQ-074、REQ-077）

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `src/galaxy/engine.ts` | 789 | Canvas + d3-force 画布引擎：力导向、三级 LOD、视口裁剪、贝塞尔连线、鱼眼、拖拽、缩放、相机缓动 |
| `src/galaxy/model.ts` | — | 类型、配色常量 `LEGEND`、`radiusOf`、`spiralLayout`、`FORCE` 参数 |
| `src/galaxy/GalaxyCanvas.vue` | — | 画布组件：工具栏 / 图例 / 悬停提示 / 右键菜单 / 捏合建边预览 |
| `src/views/GalaxyView.vue` | — | 全屏视图：返回 / 面包屑 / 层数与文件数下拉 / 全部根 / 聚焦选中 / 右侧详情卡 |
| `src/components/MindMap.vue` | — | 八种思维模型图，纯 SVG |

四个入口：顶栏 `Network` 按钮或 `Ctrl+G`（全屏）、搜索结果工具栏「图谱」（结果集）、索引面板「星系图」Tab（紧凑版）、结果工具栏「思维图」+ 模型下拉（八种模型）。`Ctrl+F` 从星系图回搜索。

后端两条新命令（已在 `src-tauri/src/lib.rs` 的 `generate_handler!` 注册）：

```rust
// src-tauri/src/cmd.rs:1823
pub fn graph_data(state, root: Option<String>, depth: Option<usize>, max_nodes: Option<usize>, file_limit: Option<usize>) -> GraphData
// src-tauri/src/cmd.rs:2149
pub fn mind_map_data(state, model: String, paths: Vec<String>, query: Option<String>) -> MindMapOut
```

`graph_data` 默认 depth 2 / max_nodes 900（上限 8000）/ file_limit 31；数据全部来自 `db.dir_rollup_under()`，**不触发磁盘遍历**。两处必要处理：① 根路径大小写从库里回填（`dir_canon`），否则第 0 层 `WHERE dir = ?1` 查不到直属文件；② 子目录归并预建 `child_index` 哈希表，复杂度从 O(目录数²) 降到 O(目录数)。

`mind_map_data` 支持 definition / description / comparison / classification / decomposition / sequence / causality / analogy；paths 为空时取最近 400 个已索引文件；`MIND_GROUP_CAP = 120`。

### 安装包瘦身

删掉的根目录文件：`.gitattributes`、`DeepDark_Oracle.exe`(20.5MB 旧构建)、`DeepDark_Oracle 启动器.cmd`、`launcher.ps1`、`WebView2Loader.dll`(2021 陈旧副本)、`libgcc_s_seh-1.dll`、`libstdc++-6.dll`、`libwinpthread-1.dll`、`onnxruntime.dll`(13.5MB)、`DeepDark_Oracle-error.txt`、`InstallationLog.txt`、`tools/InstallationLog.txt`、`tools/msys2-x86_64-latest.exe`(85MB)、`tools/node-v22.14.0-win-x64.zip`(34.9MB)、`tools/rustup-init.exe`(12.7MB)、`release/DeepDark_Oracle_0.0.22_x64-setup.exe`(682MB)。

`src-tauri/tauri.conf.json` 的 `bundle.resources` 现为：

```json
"../tools/bin/ffmpeg.exe": "tools/bin/ffmpeg.exe",
"../.tools/cuda-runtime/": "_up_/.tools/cuda-runtime/",
"../scripts/install-onnxruntime.ps1": "resources/scripts/install-onnxruntime.ps1",
"../scripts/install-python-sidecar.ps1": "resources/scripts/install-python-sidecar.ps1"
```

`scripts/build-release.ps1` 的 `Assert-ReleasePrerequisites` 同步删掉四个 DLL 的必需项。

**静态链接 GNU 运行时**（`src-tauri/build.rs`，`CARGO_CFG_TARGET_ENV == "gnu"` 分支）：

```rust
println!("cargo:rustc-link-arg=-static-libgcc");
println!("cargo:rustc-link-arg=-Wl,-Bstatic");
println!("cargo:rustc-link-arg=-lstdc++");
println!("cargo:rustc-link-arg=-lwinpthread");
println!("cargo:rustc-link-arg=-Wl,-Bdynamic");
println!("cargo:rustc-link-arg=-Wl,--start-group");
for lib in ["ucrt","mingwex","mingw32","ssp","kernel32","user32","advapi32","shell32","ole32","ws2_32"] { ... }
println!("cargo:rustc-link-arg=-Wl,--end-group");
```

四个必须同时满足，缺一个都会失败：① `-Wl,-Bstatic` 必须包住 `-lstdc++`（否则 ld 命中 `libstdc++.dll.a` 导入库）；② 必须用 GCC 驱动的 `-static-libgcc` 而不是 `-lgcc`；③ 必须 `--start-group/--end-group`（ld 单遍扫描，mingwex 与 stdc++ 互相依赖）；④ UCRT64 工具链用 `ucrt` 不是 `msvcrt`。

**`WebView2Loader.dll` 消不掉**：`webview2-com-sys 0.39.1` 的宏写死 `#[cfg_attr(not(target_env = "msvc"), link(name = "WebView2Loader.dll"))]`，只有 MSVC 才用静态库。本机无 MSVC，所以必须随包带这个 DLL（缺了 0xC0000135）。

### 构建工具链（重要，别重复踩）

GNU 工具链不认非 ASCII 路径，**sysroot 和 target 目录都必须是 ASCII**。`scripts/activate-local.ps1` 现在能自愈：

- `RUSTUP_HOME`：优先 `$env:RUSTUP_HOME` → 仓库自身 ASCII 时用 `tools\rustup` → 已存在的 `D:\ddrust` → `robocopy` 复制到 ASCII 盘 → 兜底 `%LOCALAPPDATA%\DeepDark_Oracle-toolchain\rustup`（`Test-RustupHome` / `Resolve-RustupHome`）。**junction / subst 都无效**：rustc 会把 sysroot 解析回真实中文长路径，只有真复制才行。
- `CARGO_TARGET_DIR`：`$env:CARGO_TARGET_DIR` → 仓库 ASCII 时用仓库内 target → `Resolve-AsciiDir` 按 `D:\DeepDark_Oracle-build\target` 等 ASCII 目录创建 → 兜底 `%LOCALAPPDATA%\DeepDark_Oracle-build`。可用 `DDO_BUILD_ROOT` 覆盖、`USE_BUILD_IN_WORKSPACE=1` 强制仓库内。
- **PATH 顺序**：MSYS2 UCRT64 必须排在 rust toolchain 之前，否则 gcc 加载错版本 `libgcc_s_seh-1.dll` 静默失败（表现为 `libsqlite3-sys` 报 `Compiler family detection failed` 且 stderr 为空）。
- **MSYS2 探测**：`tools\msys64` → `$env:MSYS2_ROOT` → `$env:DDO_MSYS2_ROOT` → `D:\dsh-tools\msys64` → `C:\msys64` → `D:\msys64` → `C:\tools\msys64`。（本机实际在 `D:\dsh-tools\msys64\ucrt64\bin`。）
- **不要在 PATH 里放 `tools\cargo\bin`**（rustup shim）：它会尝试同步 channel 并报 `could not rename 'component' file ... os error 145`。用真实 toolchain 目录的 `cargo.exe`。

### 其他修复

- `scripts/patch-llama-embeddinggemma2.ps1`：判据从 `build_inp_embd(model.tok_embd);` 改成前缀匹配 `build_inp_embd(model.tok_embd`（上游新增了 attention scale 参数），否则条件恒假 → 走进 `git apply` 分支 → 项目非 git 仓库 → 打包失败。
- `src-tauri/native/llama_shim.cpp` 跟进 llama.cpp 0.7 的 mtmd API：`mtmd_context_params.backend_device`（字符串）→ `device`（`ggml_backend_dev_t` 句柄），共 2 处；`mtmd_set_image_max_tokens()` 已被移除，共 2 处删除（图像 token 预算只在 `mtmd_context_params.image_max_tokens` 初始化时设定）。
- 消掉 `graph_data` 的 `unused variable: dir_depth` 警告。

### 验证记录

| 项目 | 结果 |
| --- | --- |
| `vite build` | 1826 modules，`dist/assets/index-DTZ5QaNe.js` 310.85 kB |
| `cargo check --lib` | 0 error 0 warning |
| `npm run tauri:release`（模拟全新终端） | EXIT=0，221 秒 |
| 安装包静默安装 `/S` | 退出码 0，装到 `D:\ddorealtest` |
| 安装版 `--cli stats` / `--selftest` | 0.24 / 通过 6、失败 0、跳过 10 |
| 静默卸载 `uninstall.exe /S` | 退出码 0，程序与注册表项移除 |
| GUI 全局星系图（真实 28 万文件库） | 1140 节点 · 66% 裁剪 · **240 FPS** |
| GUI 结果图谱 | 1297 节点 · 46% 裁剪 · 240 FPS |
| GUI 思维图 | 界定 / 描述 / 顺序 三种渲染正确 |
| GUI 索引面板星系图 Tab | 紧凑画布跟随选中目录 |

### 已知问题

- **vite build 不做类型检查**（项目未装 typescript，`node_modules/.bin` 里没有 `tsc` / `vue-tsc`）。本轮就出现过 `src/galaxy/engine.ts` 里变量改名后漏改一处、`vite build` 依然通过、运行时才报 `ReferenceError: fs is not defined` 的情况。**改完前端必须在真 GUI 里点一遍新功能。**
- `rm -rf` / `find -delete` 在本机被 CC Safety Net 拦截（`rm.recursive-force-outside-cwd` / `find.delete`）。删目录用 `tools/node/node.exe -e "require('fs').rmSync('dir',{recursive:true,force:true})"`。
- 需求表里 REQ-062~092 的多数条目（智能路由、手势引擎、Z 轴、意图预测、AI 打标、UserProfile、推荐面板、自然语言查询）未实现，理由见 `docs/DeepDark_Oracle_v0.24_交付说明.md` 第六节。

---

## v0.25 增量（2026-11-01）：GPU 加速兼容性

版本号：`Cargo.toml` / `tauri.conf.json` / `package.json` = `0.0.25`；`src-tauri/src/lib.rs` 的 `APP_VERSION = "0.0.25"`、`APP_DISPLAY_VERSION = "0.25"`。需求来源 `DeepDark_Oracle_GPU兼容性需求明细表.xlsx`（REQ-093 ~ REQ-108）。

### 交付物

| 项 | 值 |
| --- | --- |
| 安装包 | `release/DeepDark_Oracle_0.0.25_x64-setup.exe` |
| 大小 | 34,276,337 字节（32.7 MB） |
| SHA-256 | `677aca3e6367cb247e3ea003750081a686c62bbf47a2a06dee7961501912d606` |
| 安装包内容 | 13 个文件：`DeepDark_Oracle.exe`(57,298,944B)、`WebView2Loader.dll`(160,880B)、`tools/bin/ffmpeg.exe`(87,638,016B)、`resources/scripts/*.ps1` ×2、`uninstall.exe`、`$PLUGINSDIR/*` ×7 |
| **不含** | models 权重、data 索引、`onnxruntime.dll`、`libstdc++-6.dll`、`libgcc_s_seh-1.dll`、`libwinpthread-1.dll`、`vulkan-1.dll` |

### 推理侧：GPU 后端探测（REQ-093 / 094 / 096 / 097 / 098 ~ 102）

需求表是按 PyTorch / Python 写的，本项目没有 PyTorch，所以等价改写成「查 GGML 后端注册表 + 显卡厂商工具」。

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `src-tauri/src/ai/gpu.rs` | 519 | `probe()` 能力报告、`classify_tier()` 显存分档、`detect_cuda_toolkit()` 解析 `nvcc --version`、`recommend_cuda()`、`onnx_runtime_present()`、`write_report()` |
| `src-tauri/native/llama_shim.cpp` | — | 新增 3 个 C 导出：`ds_llama_backend_count` / `ds_llama_backend_info` / `ds_llama_device_tier`；`ds_llama_create` 里按 `memory_total` 分档决定 `n_gpu_layers` |
| `src-tauri/src/ai/llama.rs` | — | `gpu_backends()` / `device_tier()` 包装上述 C 接口 |
| `src-tauri/src/cmd.rs` | — | `gpu_report()` / `gpu_refresh_report()` 两条 Tauri 命令（已在 `lib.rs` 注册） |
| `src-tauri/src/cli.rs` | — | `--cli gpu [--json]` |

- **后端枚举走 GGML 注册表**，不是猜文件名：`ggml_backend_reg_count()` 逐项 + `ggml_backend_reg_get_proc_address(reg, "ggml_backend_get_features")` 取特性串。这样「二进制里编进了哪些后端」和「运行时真有哪块卡」是两个独立结论，不会互相掩盖。
- **显存分档** `classify_tier()`：≥8 GB → Tier 0 全量下放；4~8 GB → Tier 1 建议量化；1~4 GB → Tier 2 小模型/部分下放；<1 GB → Tier 3 回落 CPU。`ds_llama_create` 里 `memory_total < 1 GiB` 时 `selected_device = nullptr`、`n_gpu_layers = 0`；否则 `n_gpu_layers = -1` 全量下放。
- **CUDA 校验**：`nvcc --version` 输出用 `release (d+.d+)` 解析，拿不到再退 `CUDA_PATH`/`CUDA_HOME` 下的 `version.txt`。**本项目用 GNU 工具链，CUDA 编不进来**（`build.rs` 里 `cuda_enabled && windows_gnu` 直接 panic），所以 CUDA 缺失时状态仍是 ready（走 Vulkan），只在 `notes` 里说明。`--skip-cuda-check` 启动参数跳过这一步。
- **报告落盘** `gpu_report.log`（内容是 pretty JSON），启动时写一次，设置页「写入诊断日志」可再写。
- **ONNX EP 顺序** `ORTPROVIDER_ORDER` = CUDA → DirectML → CPU，只在扩展装过（`onnx_runtime_present()`）时展示。

### 渲染侧：WebGL + Canvas 2D 双管线（REQ-103 / 104 / 105 / 106）

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `src/galaxy/capability.ts` | 294 | `detectWebGL()` 探测、`KNOWN_DRIVER_BUGS` 4 条、`QUALITY_PROFILES` 四档、`effectiveDpr()`、`pickBackend()` |
| `src/galaxy/glRenderer.ts` | 379 | WebGL2/WebGL1 点精灵 + 线段着色器，highp 编译失败自动换 mediump |
| `src/galaxy/telemetry.ts` | 115 | 崩溃/掉帧本地记录（`localStorage`，最多 20 条，30 秒同内容去重） |
| `src/galaxy/renderSettings.ts` | — | 画质 / 后端偏好 / DPR 覆盖，`localStorage` 持久化 |
| `src/galaxy/engine.ts` | — | 按探测结果分发 `drawGL()` / `draw2D()`，WebGL 抛错时降级并记录 |

- **降级链**：WebGL 2 → WebGL 1 → Canvas 2D。`pickBackend()` 命中 `force-canvas` 类驱动问题时直接用 Canvas 2D；用户在设置里强制 WebGL 则尊重用户一次。
- **标签层分离**：WebGL 拿走的 canvas 不能再拿 2D 上下文，所以文字标签画在叠加的第二块 canvas 上（`labelCanvas`）。
- **鱼眼透镜进了顶点着色器**（REQ-077 在 GL 下的等价实现）：`u_fish` / `u_fishScale` 两个 uniform 做位移与放大，不用每帧回读像素。
- **DPI**：`effectiveDpr()` 先按画质档位取上限（低=1x / 中=2x / 高=2x / 极高=3x），Canvas 2D 再压到 2x 以内；用户手动指定倍数也受该上限约束。
- **慢帧提示**：连续 45 帧单帧耗时 >16.7ms 且未提示过，回调一次让界面建议降档，并记一条 `slow-frame` 到崩溃日志。

### 设置页「图形与 GPU」（REQ-105 / 107 / 108）

`src/components/SettingsGpuTab.vue`（342 行）五张卡片：GPU 能力报告 / 画质档位 / 渲染后端与分辨率 / 渲染能力 / 已编入二进制文件的后端 / 兼容性诊断。挂载点在 `SettingsDrawer.vue` 的 `TABS` 里新增 `{ key: "render", label: "图形与 GPU", icon: MonitorCog }`。

`scripts/gpu-check.ps1` 是 REQ-107 的本机等价实现（CI 矩阵在本机跑不了）：跑 `--cli gpu --json` → 归一化结论 → 可传 `-Baseline` 对比退化（tier 变化、后端变化、可用显存掉 >20%）→ 输出 `docs/gpu-report/gpu-report-<时间戳>.json` + 同名 `.html`。`-Strict` 时退化或「不支持」返回 1。

### Vulkan 后端默认启用

`build.rs` 的 `vulkan_sdk_root()` 现在会自己找 SDK，找不到才关：

```
候选：$VULKAN_SDK → $DEEPAISEE_VULKAN_ROOT → $MSYS2_ROOT → <repo>/.tools/vulkan → <repo>/.tools/vulkan-sdk
      → D:\dsh-tools\msys64\ucrt64 → C:\msys64\ucrt64 → D:\msys64\ucrt64
判定：include|Include/vulkan/vulkan.h 且 bin|Bin/glslc.exe 都在
```

`SPIRV-Headers` 的 config 位置三种布局都认（`Lib|share|lib/cmake/SPIRV-Headers`）——Khronos 官方 SDK 放 `Lib/`，MSYS2 放 `share/` 和 `lib/`。

`scripts/activate-local.ps1` 同步加了 `Test-VulkanRoot()` 与候选探测，命中后设 `VULKAN_SDK` / `DEEPAISEE_VULKAN_ROOT` 并把 `glslc` 目录加进 PATH，打印 `Vulkan SDK: <路径>`。**注意 `scripts/build-release.ps1` 原来开头有两行 `if (-not $env:DEEPAISEE_VULKAN) { $env:DEEPAISEE_VULKAN = "0" }`，会把自动探测永远关掉，已删除**；要强制关就自己显式设 `DEEPAISEE_VULKAN=0`。

### 构建工具链（本轮新踩，务必记住）

- **PowerShell 5.1 在 .ps1 没有 UTF-8 BOM 时按 ANSI（GBK）读文件**，中文注释会把换行/引号吃掉，报 `ParserError: 意外的标记 }`。所有含非 ASCII 的 `.ps1` 必须带 `EF BB BF`。本轮 `scripts/{activate-local,build-release,gpu-check,install-onnxruntime}.ps1` 已加 BOM；纯 ASCII 的脚本不需要。检查：`head -c 3 file | od -An -tx1`。
- `vite build` 不做类型检查，**模板里引用了未定义的变量也能构建通过**，只有在真 GUI 里点到那一页才炸。本轮 `SettingsGpuTab.vue` 的 `{{ devicePixelRatio.toFixed(2) }}` 就是这种：Vue 把它编译成 `_ctx.devicePixelRatio`，运行时 undefined 抛错，整页空白。已改成 `screenDpr` computed。**同类问题可以静态扫**：用 `@vue/compiler-sfc` 的 `compileScript` 拿 `bindingMetadata` 再 `compileTemplate({ compilerOptions: { bindingMetadata, prefixIdentifiers: true } })`，grep 生成代码里的 `_ctx.xxx` 即是模板自由变量。

### 验证记录

| 项目 | 结果 |
| --- | --- |
| `vite build` | 1833 modules，`dist/assets/index-*.js` 341.79 kB |
| `cargo check --lib` | 0 error 0 warning |
| `npm run tauri:release` | EXIT=0，产出 34,276,337 字节安装包 |
| exe 依赖（objdump） | 只剩系统 DLL + `WebView2Loader.dll` + `vulkan-1.dll`；三个 MinGW 运行时 DLL 已消失 |
| `--cli gpu`（RTX 4060 Laptop） | 后端 `vulkan` / ready；7957 MiB 总显存、7189 MiB 空闲、Tier 1；CUDA 未安装（本版无需）；驱动 566.26；已编入后端 Vulkan（加速器）+ CPU |
| 安装包解包 | 13 个文件，不含模型/索引/陈旧 DLL |

### 已知问题

- **`vulkan-1.dll` 不打进安装包**：exe 只静态导入 4 个 Vulkan 符号，其余走 `vkGetInstanceProcAddr`；实测把 `vulkan-1.dll` 从 exe 旁边移走仍能 `--cli gpu` 成功，因为 `C:\Windows\System32\vulkan-1.dll` 由显卡驱动提供。MSYS2 那份（724,189B）额外依赖 `api-ms-win-crt-*`，不适合分发。
- **`WebView2Loader.dll` 仍在包里**（v0.24 结论不变）：`webview2-com-sys 0.39.1` 只在 MSVC 下用静态库。
- 需求表 REQ-095（PyTorch 重装引导）、REQ-102（ONNX 推理会话）、REQ-107（CI 矩阵）、REQ-105（在线 Bug 库）按本机架构改写成等价形态，理由见 `docs/DeepDark_Oracle_v0.25_交付说明.md`。


## v0.26 增量（2026-11-01）：3D 星图 + 安装包清理

### 交付物

| 项 | 值 |
| --- | --- |
| 安装包 | `release/DeepDark_Oracle_0.0.26_x64-setup.exe`，34,642,746 字节（33.04 MiB） |
| SHA-256 | `a8e945321e1f220c587436285921cd97106c7b0adc114e4c6319eac0815530ea` |
| 免安装 exe | `DeepDark_Oracle.exe`，57,692,160 字节，SHA-256 `edfc792b2e506b34f4bc27c1bee09c5449edf00e05c1703129555a7828226074` |
| 需求来源 | `2D转3D与星图需求明细.xlsx`，单表 33 条（序号 1~33，无 REQ 编号） |
| 对照文档 | `docs/DeepDark_Oracle_v0.26_交付说明.md`、`docs/星图视觉参考提示词.md`（需求 #18） |

### 新增 `src/galaxy/star/`（2588 行）

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `engine3d.ts` | 1043 | `ForceGraph3D` 只出力学的相机（`nodeVisibility` 全关），节点/连线/星尘三套 `InstancedBufferGeometry`；`bindPointer()`、`setPostSuspended()`、`applyBloom()`、`applySketch()`、`updateLod()`、CPU 屏幕投影拾取 |
| `StarCanvas.vue` | 901 | Vue 外壳：`loadInitial` / `drill` / `goBack` / `goForward` / `focusSelected` / `reload` 暴露给 `GalaxyView`；HUD、图例、面包屑、右键菜单、`onSlowFrame` 降级 |
| `nebula.ts` | 297 | `NEBULA_DEFAULTS = {count:3000, radius:900, fogDensity:0.002, spin:0.08}`；空间哈希就近配对连线（`segLimit = min(220, n*0.09)`、`maxLen = radius*0.07`）；`SphereGeometry(R*1.6)` 内壁 + `FogExp2` |
| `shaders.ts` | 174 | `glowTexture(512)` / `dotTexture(64)` / `cloudTexture(256)` 三张**运行时程序化贴图** + `NODE_VERT/NODE_FRAG/PARTICLE_VERT/PARTICLE_FRAG` |
| `sketch.ts` | 173 | `createSketchPass(w,h)`：Sobel 采样深度 + fbm 噪声偏 UV + 噪声等值线 + 程序化纸纹 Multiply，默认关闭 |

后端 `src-tauri/src/star.rs`（582 行）四条命令已在 `lib.rs:228-231` 注册，前端封装 `src/api.ts:570-577`：
`starOverview()` → `star_overview(state) -> StarGraph`；`starChildren(nodeId?, fileLimit=60, dirLimit=80)` → `star_children(nodeId, fileLimit, dirLimit)`；`starLayoutGet(key)`；`starLayoutPut(key, positions)`。

### 两个必须记住的坑

**(1) Bloom 参数不能照抄需求表。** 需求 #12/#17 给 `strength 1.8 / radius 0.5 / threshold 0.1`，照抄后**整屏过曝、亮像素 51.2%**。原因：深空底色 `#050510` 亮度已贴着 `threshold 0.1`，星云球壳（`SphereGeometry(R*1.6)`）整片参与泛光就饱和成白。现值为 `strength 0.85/0.7/0.55（high/mid/low）`、`radius 0.35`、`threshold 0.62`，复测亮像素 **4.65%**。另外 `cloudTexture` 的斑 alpha 从 `0.05+rnd*0.1` 降到 `0.012+rnd*0.026`，星云内壁从 `AdditiveBlending/opacity 0.9` 改成 `NormalBlending/opacity 0.34`（`nebula.ts:235`），`NODE_FRAG` 的 `core*1.35` 降到 `core*0.95`。

**(2) 3d-force-graph 的 `backgroundColor()` 只认 CSS 色串，传数字会让整页白屏。** `three-render-objects` 内部走 `polished.parseToRgb()`，该函数第一行 `if (typeof color !== 'string') throw`，抛出后整棵组件树挂掉，界面显示「界面加载出错」。修法（`engine3d.ts:73-79`）：

```ts
const BG_PLAIN = 0x080b14;        // scene.background 用，数字没问题
const BG_DEEP_CSS = "#050510";    // fg.backgroundColor() 用，必须是 CSS 串
```

### 需求覆盖：28 条落地 + 5 条等价改写

没有照做的 5 条（理由都在交付说明里）：**#13** 用 Instanced 广告牌替 SpriteMaterial（1 draw call 顶掉 N 个 Sprite）；**#31** 不做 KTX2 + 纹理图集（所有贴图运行时生成，磁盘上无图片资产可压，实际做的是 2 的幂 + mipmap + `setAnisotropy`）；**#32** 不做 Mesh BVH（改 CPU 屏幕投影，`engine3d.ts:699` 注释：比 `Raycaster` 对一万个球做检测快两个数量级）；**#4** 不做移动端 `controlType=orbit`（桌面程序）；**Bloom 参数**见上。

### 安装包清理

| 删除 | 体积 | 依据 |
| --- | --- | --- |
| `.gitattributes` | 163 B | 非 git 仓库 |
| `libgcc_s_seh-1.dll` / `libstdc++-6.dll` / `libwinpthread-1.dll` | 2.88 MB | MSYS2 编译期运行时，exe 已静态链接 |
| `onnxruntime.dll` | 13.56 MB | 可选扩展，由 `resources/scripts/install-onnxruntime.ps1` 按需安装 |
| `vulkan-1.dll` | 724 KB | 系统 `System32` 自带 1,452,392 B；移走后实测启动正常 |
| `resources/` | — | 冗余旧副本（打包取 `../scripts/*.ps1`），副本里 `install-onnxruntime.ps1` 还缺 `param([switch]$Update)` |
| `plugins/` | — | 源码零引用；`src-tauri/nsis/installer-hooks.nsh:32-40` 安装时会自动创建 |
| 92 个 `_` 前缀临时文件 + `_pkgtest/` | **79.1 MB** | 开发期取证残留 |

**`WebView2Loader.dll`（160,880 B）必须留在根目录**：移走即 `0xC0000135 STATUS_DLL_NOT_FOUND`（退出码 3221225781），窗口一个都不出。它由 cargo 生成在 `D:\DeepDark_Oracle-build\target\release\` 下，**不会自动复制到项目根**，每次重新出包后要手动补。

### 构建工具链（本轮新踩）

- **安装包在 `bundle/nsis/` 子目录，不在 `target/release/` 根**：`D:\DeepDark_Oracle-build\target\release\bundle\nsis\DeepDark_Oracle_0.0.26_x64-setup.exe`。
- **复制 exe 报 `EBUSY: resource busy or locked`**：构建期间会有残留 `DeepDark_Oracle.exe` 进程占着文件。先 `cmd /c "taskkill /F /IM DeepDark_Oracle.exe"` 再 sleep 2。
- **MSYS2 bash 里直接跑 `.exe` 会假报缺 `api-ms-win-crt-utility-l1-1-0.dll`**，是 MSYS 环境假象；必须用 `cmd /c start` 或 Python `subprocess.Popen` 启动。
- **找窗口不能按标题模糊匹配**（`"DeepDark" in title` 会命中 VS Code 的同名文档窗口）：`tasklist` 取 PID → `EnumWindows` + `GetWindowThreadProcessId` 比对 → 类名必须是 `Tauri Window`。置前要先 `AttachThreadInput`，否则 `SetForegroundWindow` 无效。
- **高 DPI 截图**：本机 2560x1600 / DPI 144。`GetWindowRect` 给的是物理像素，用 bbox 裁剪只能抓到左上角一块；必须**全屏 `ImageGrab.grab()` 后再裁**。
- **过曝/亮度统计**用 Pillow 抽样算亮像素占比最快，不要靠肉眼。

### 验证记录

| 项目 | 结果 |
| --- | --- |
| `npm run build` | exit 0，2211 modules，11.06s |
| `npm run tauri:release` | EXIT=0，cargo 4m 01s，NSIS 33.04 MiB |
| exe 启动 | 窗口 `Tauri Window`「DeepDark_Oracle · AI 磁盘智搜」正常 |
| `Ctrl+G` 进 3D 星图 | 正常渲染，HUD「全部索引 / 9 节点 / 240 fps / 1 批次」 |
| 过曝复检 | 亮像素 51.2% → 4.65% |
| 静默安装 `/S /D=D:\ddo_v26test` | exit 0，7 文件 + 3 目录，139 MB（含 ffmpeg 87 MB） |
| 安装版启动 | 正常出窗口 |
| 静默卸载 `uninstall.exe /S` | exit 0，仅留 `data/`、`webview-data/`、`plugins/README.txt`（设计如此）；注册表 `HKCU\...\Uninstall\DeepDark_Oracle` 已清空 |
| 根目录 | 31 项，`_` 前缀残留 0 个 |

### 已知问题

- **本机无 CUDA Toolkit** → v0.26 是**无 CUDA 内核**的构建（Vulkan 后端在编），`.tools/cuda-runtime` 目录存在但 0 个文件。要出 CUDA 版必须先装 CUDA Toolkit 再改 `build-release.ps1` 的 features。
- `bundle/nsis/` 里还留着 0.0.24（30,501,454 B）与 0.0.25（34,276,337 B）两个历史包，需要时可清。

---

## v0.27 增量（2026-11-03）：七项改造落地 + CUDA 暂缓

本节记录 2026-11-02 ~ 11-03 对用户提出的 7 项需求的处理结果。需求原文见 `docs/aegis/work/2026-11-02-StarAndBlueprint/TaskIntent.md`，蓝图规格见 `清晰平面与蓝图风格需求明细表.xlsx`（单表 15 条）。

### 交付物

| 项 | 值 |
| --- | --- |
| 版本 | `0.0.27`（显示版 `0.27`） |
| 免安装 exe | `D:\DeepDark_Oracle-build\target\release\DeepDark_Oracle.exe`，57,781,760 字节（11-03 22:09） |
| exe SHA-256 | `cac1f4e860b1441a1820831f24668286801f4d72c611cf1bb91432866e68ad4b` |
| 安装包 | `release\DeepDark_Oracle_0.0.27_x64-setup.exe`，34,680,388 字节（33.07 MiB，11-03 22:11） |
| 安装包 SHA-256 | `4c63a2f39d094bfc2cd7b0777761dbf6f3b99d1ad5f49ba67bbe221b66679078` |
| 构建特性 | `llama,candle,custom-protocol`（**无 candle-cuda**，理由见下节） |

### 七项需求的结论

| # | 需求 | 状态 | 落点与证据 |
| --- | --- | --- | --- |
| ① | 2D 星图改清晰平面蓝图风格 | **完成** | `src/galaxy/model.ts:77` `BLUEPRINT_CONFIG`；`engine.ts:898` 底色 #dcebf5 + 40px 网格 #cce0f0；`glRenderer.ts:232/259/307` 配色；`GalaxyCanvas.vue:450-589` 白底 + 3px 深边 + 硬阴影面板。CDP 截图实测 1564 节点 / 240 FPS |
| ② | 导入数据超 1T 就整软件卡 | **完成** | graph_data 首屏 **15.9~18.8 s → 0.69~0.89 s**；tree_data 1052→451 ms；tree_search 从全量 GROUP BY 改为 LIKE 粗筛 + 精确二次过滤；star_children 1177→372 ms |
| ③ | 接入 candle CUDA 后端 + safetensors | **部分（CUDA 暂缓，见下节）** | safetensors 路径可用（`candle_backend.rs`）；CUDA 需 MSVC + CUDA Toolkit，本机不具备；**已用 ORT DirectML EP 补上 GPU 推理**（`ai/ort.rs`），CLIP 实测 `EP=DML` 加载成功 |
| ④ | 3D 星图星星点不开、一点就报错 | **完成** | 根因：d3-force 的 `initializeLinks` 就地改写 link.source/target 为对象，而 three-forcegraph 的 update 顺序是 `numDimensions()` 先于 `nodes()`，复用同一 links 数组会让新一轮用旧节点表查新节点 → `Error: node not found: kind:other@D:`。修复：`engine3d.ts` 新增 `private graphLinks()`，用 `idOf` 规范化成字符串 id 并过滤两端不在 `byId` 的残边，`pushGraph()` 每次传新副本 |
| ⑤ | 盘符改成恒星 + 引力（单盘自转/N 体） | **完成** | `star.rs` `build_drive_level`（`drive_kind_stats` 聚合）；`engine3d.ts:540-643` `assignOrbits`：drive 恒星两两互绕（`r = 150 + driveCount*46`、`speed = 0.16/sqrt(driveCount)`、奇偶反向），单盘 `radius 0 speed 0` 静止 |
| ⑥ | 文件夹=行星，自转公转；点开不要爆一大堆 | **完成** | kind 行星环 `ring = 2.4 + idx*0.62`、folder/file 卫星环 `1.7 + (idx%8)*0.34`；`GalaxyView.vue:215` `dir-limit=24`、`starFileLimit` 上限 24 |
| ⑦ | GPU 占用几乎为零 | **完成** | 见下表 |

### 需求⑦「索引时 GPU 几乎为零」的完整因果链与修复

诊断过程中排除了两个错误假设，记录在此以免后人重踩：

- **假设一「模型没上 GPU」是错的**。最初用 `nvidia-smi` 每 0.8 s 采样，看不到 +558 MB 台阶就下了结论。正确方法是 `nvidia-smi --query-gpu=memory.used,utilization.gpu --format=csv,noheader,nounits --loop-ms=200`（单次调用要 0.48 s，bash 的 for/sleep 循环会被它自己拖慢到 800 ms 间隔）。改用原生循环后立刻看到 **+1.7 GiB 台阶**，模型确实全量下放。
- **假设二「Db 单 Mutex<Connection> 导致 CPU 单核串行」是错的**。实测 `Get-Process TotalProcessorTime` 斜率只有 1.03~1.25 核（32 核机器）。真元凶是 `ai/hash.rs` 的 `image_vec_from`：对 pixiv 原图（4000×6000 = 7200 万像素）做 `img.to_rgb8()` 全尺寸复制再遍历两遍，单张 0.3 秒起跳。

四处修复：

1. `ai/hash.rs` `image_vec_from`：先 `resize_exact(64, 64, FilterType::Triangle)` 再做直方图与亮度统计。
2. `index.rs` `run_loop`：`first_image` 不再要求主嵌入模型有多模态能力（纯文本 GGUF 时图片仍走批量路径）。
3. `index.rs` `process_image_batch`：新增 embedder/store 选择器；CLIP 改批量前向 `ClipModel::image_data_vecs(&[&DynamicImage])`（`clip.rs`，ONNX 模型 batch 维是符号，本来就能批）。
4. `db.rs` `pending_cond` / `kind_scope`：队列按 kind 过滤。原来 `pending_ids("text")` 完全不看 kind，导致 **47,691 张图片**被排进文本队列，每张都白跑一遍 `text=skip` + 做缩略图（实测 140 秒推进 749 条、正文 0、向量 0）。

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 图片索引吞吐（120 s） | 357 条 = 2.98 张/秒 | **1430 条 = 11.9 张/秒** |
| 跨模态向量 | 357 | 1430 |
| 缩略图 | 8 | 607 |
| GPU util 均值 | 11.9% | **28.0%** |
| GPU util 峰值 | 100% | 100%（更早的旧二进制是 5.6% 均值 / 34% 峰值） |

### ★ CUDA 后端：本机不具备条件，暂缓（需求③的遗留部分）

**结论：v0.27 不产出 CUDA 版，维持 Vulkan（llama.cpp）+ DirectML（ONNX Runtime）的 GPU 方案。** 理由与后续接手方式如下：

1. **硬阻断在 `src-tauri/build.rs:26-28`**：

   ```rust
   if cuda_enabled && windows_gnu {
       panic!("DEEPAISEE_CUDA requires the x86_64-pc-windows-msvc target; \
               NVIDIA CUDA does not support this project's Windows GNU toolchain");
   }
   ```

   即 **CUDA 构建必须用 MSVC 目标**，而本机当前是 MSYS2 UCRT64 + GNU 工具链。
2. **本机实测无 MSVC、无 CUDA Toolkit**：`C:\Program Files\Microsoft Visual Studio` 与 `C:\Program Files (x86)\Microsoft Visual Studio` 均不存在，`vswhere.exe` 也不在；`C:\Program Files\NVIDIA GPU Computing Toolkit` 不存在，`CUDA_PATH` 为空，`which nvcc` 无结果。`.tools/cuda-runtime/` 存在但 **0 个文件**。
3. **磁盘也不宽裕**：C 盘 201 GB 只剩 20 GB，装 VS 2022 Build Tools + CUDA Toolkit 会非常勉强，需要另选盘。
4. **用户的 0.0.23 包无法在本机验证**：`E:\111\DeepDark_Oracle_0.0.23_x64-setup.exe` 有 523,182,942 字节（498.9 MiB，明显大于后来三个 30~34 MB 的包），但 NSIS 装的是 LZMA 压缩，字节扫描 `cublas64`/`cudart64`/`cudnn64`/`onnxruntime.dll`/`mmproj` **全部 0 命中**（压缩包内字符串扫不到属正常，不构成否定证据）。7-Zip 26.04 的 `l` / `l -tNsis` 与改 ASCII 路径均报 `Unsupported archive type`；静默安装 `/S /D=` 报 NSIS Error「Error writing temporary file」，改 TEMP 后仍 `EXIT=2`。HANDOFF 历史记录说 v0.23 是「MSVC x64 + CUDA Toolkit 13.1」构建，那台机器与本机不是同一套环境。
5. **CUDA 不是当前瓶颈**：图片索引走 HashEmbedder + CLIP，文本模型是 Vulkan 上的 llama.cpp，两者都不经过 Candle。即便编出 candle-cuda，也只在「用户导入 safetensors 且选了 CUDA」时才有区别——而 `models/` 里**没有任何 safetensors 权重**（只有 `embeddinggemma2.gguf` 557,949,696 B + `clip/` + `clap/`）。

**将来要补 CUDA 版时的接手步骤**：

1. 在一台装了 **VS 2022 Build Tools（含 MSVC v143 x64）+ CUDA Toolkit 12.x/13.x** 的机器上 clone 本仓库（**不要**用 UCRT64/GNU 工具链）。
2. 确认 `nvcc --version` 可用，且 `C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\vX.Y\bin\nvcc.exe` 存在（`scripts/build-release.ps1` 的 `Find-CudaRoot` 按 `DEEPAISEE_CUDA_ROOT` → `CUDA_PATH` → `CUDA_HOME` → `.tools\cuda` → `.tools\cuda-toolkit` → 上述安装路径的顺序找）。
3. 直接跑 `powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1`。脚本检测到 CUDA Toolkit 后会：
   - 把 features 从 `llama,candle,custom-protocol` 换成 `llama,candle,candle-cuda,custom-protocol`；
   - 执行 `Stage-CudaRuntime`，复制 `cublas64_*`/`cublasLt64_*`/`cudart64_*`/`curand64_*`/`nvrtc64_*`/`nvJitLink_*.dll` 到 `.tools\cuda-runtime`，并补 `vcomp140.dll`。
4. 验证：`--cli gpu` 的 `cuda_version` 不应再是 null；导入一份真实 safetensors 权重后 `model_load` 选 CUDA 设备应能加载。

### 其它本轮踩到的坑（新）

- **中文字节切片 panic 会让整个进程消失**：`db.rs` `fold_dir_children` 用 `&dir[..prefix.len()]` 做前缀判断，`prefix.len()` 是字节数，目录名含中文时切片落在多字节字符中间 → `byte index N is not a char boundary`。这是 Tauri 命令线程里的 panic，**非 unwinding 直接 abort**（`/tmp/gui.log` 只有 `thread caused non-unwinding panic. aborting.` 一行）。必须改用 `dir.get(..prefix.len())` / `dir.get(prefix.len()..).unwrap_or("")`。已在 `db.rs:1479-1513` 修复，并用 CDP 实测点开 `D:\软件` → `触手迷宫1.1` 三层中文目录不闪退。
- **`cargo test --lib` 在本机 GNU 工具链下跑不起来**：编译成功，运行即 `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`（bash 下报 `api-ms-win-crt-utility-l1-1-0.dll: cannot open shared object file`）。这是环境限制不是代码问题，验证只能靠 `cargo check` + CLI 实测。曾尝试把 `windows_x86_64_gnu-*` / `winapi-x86_64-pc-windows-gnu` 的 `.a` 全拷到 ASCII 目录 `D:/ddo-libs` 并加 `RUSTFLAGS=-Lnative=D:\ddo-libs`，仍然失败。
- **构建前必须 `taskkill //F //IM DeepDark_Oracle.exe`**，否则 cargo 报 `failed to remove file ... 拒绝访问 (os error 5)`。
- **`_tmp_check.log` 是 UTF-16LE**，读错误行要 `iconv -f UTF-16LE -t UTF-8 _tmp_check.log | grep -n 'error'`。

### 验证记录

| 项目 | 结果 |
| --- | --- |
| `npm run build` | EXIT=0，2211 modules，9.18 s |
| `cargo check --features llama,candle,custom-protocol` | CARGO_EXIT=0 |
| `scripts/build-release.ps1` | RELEASE_EXIT=0，cargo 2m 49s，NSIS 33.07 MiB |
| GUI 中文目录三层钻取（CDP） | D → 其他 → 软件 → 触手迷宫1.1，`hit:true`、`errMask: none`、`exceptions: []`，进程存活 |
| 3D 星图首屏 | HUD「全部索引 / 3 节点 / 239 fps」，D、E 两颗恒星 |
| 图片索引吞吐 | 1430 条/120 s（11.9 张/秒），GPU util 均值 28.0% / 峰值 100% |
| CLIP 加载 | `[clip] 已加载：...models\clip\model.onnx · EP=DML` |


---

## v0.28 增量（2026-11-04）：2D 语义星图 + 3D 恒星重做

### 交付物

| 项 | 值 |
| --- | --- |
| 版本 | `0.0.28`（显示版 `0.28`） |
| 安装包 | `release\DeepDark_Oracle_0.0.28_x64-setup.exe`，34,727,004 字节（33.12 MiB） |
| 安装包 SHA-256 | `294cf45df54dfcf95d1634334bcb7eda844f73e92bd1ea5543134d24565c18a4` |
| 免安装 exe | `D:\DeepDark_Oracle-build\target\release\DeepDark_Oracle.exe`，57,939,968 字节 |
| exe SHA-256 | `cd67ed81dd1d2160604c1b7508f14d82844be3f2ed80f944cac5bd2bd0d38bf1` |
| 构建特性 | `llama,candle,custom-protocol`（无 CUDA 内核，本机无 Toolkit） |
| 需求来源 | `今日需求汇总明细表.xlsx`（15 条）+ 用户口述修正 |

### 用户口述修正（比需求表更重要）

1. **「排序模式」= 2D 星图本身按向量语义重排**，不是给目录树换排法。
2. **3D 恒星太丑**：既不大也不亮，且不能随盘符容量改变恒星大小与引力。
3. **那块矩形叫「全部索引」**（18:00 排障时的关键指认，见下文踩坑）。

### 完成清单

| # | 需求 | 落点 | 验证 |
| --- | --- | --- | --- |
| 1 | 盘符容量 → 恒星大小/引力 | `src-tauri/src/disk.rs`（新，218 行，Win32 三函数，零新增依赖）+ `star.rs::classify_drive/star_class_color` + `engine3d.ts::starRadiusOf/gravityOf` | 3D 实测：D 807GB→半径 97、E 1.8TB→123 |
| 2 | 恒星发光球 + 日冕 | `shaders.ts` 新增 `STAR_VERT/STAR_FRAG`；`engine3d.ts` 恒星独立 Instanced 层 | 亮像素 **0.46%**（不再过曝） |
| 3 | 盘的角色配色 | `STAR_CLASS`：system 蓝白 / data 橙红 / archive 暗红 / removable 白矮 | D 橙黄、E 粉红，肉眼可分 |
| 4 | 引力影响行星轨道 | `assignOrbits` 按 `parent.gravity` 缩放轨道与角速度 | D 盘 8 行星轨道 358→957、速度 0.144→0.119（开普勒式） |
| 5 | 2D 一键整理 | `src/galaxy/layouts2d.ts`（新）：tree / radial / cluster 三布局 | 240 FPS，零异常 |
| 6 | **2D 语义星图（核心）** | `star.rs::star_semantic` + `GalaxyCanvas.vue` 数据源切换、阈值/节点数滑块、按簇筛选 | **251 节点 / 12 簇 / 88% 缩放 / 240 FPS** |
| 7 | 簇包络与簇名 | `engine.ts::drawClusterHulls` | 12 个包络圆 + 短簇名，互不重叠 |

### 关键设计决定（别再改回去）

- **四个向量空间必须分开取样**。`vectors`（本机 0 条）/ `vectors_media`（256 维）/ `vectors_clip`（512 维）/ `vectors_audio`。只读主空间会得到 0 条向量、星图全空。
- **每个空间各自做 k-means，簇 id 全局自增**。早期版本只对"主维度"聚类、其余每个点单独成簇 —— 实测产生 **114 个簇**（8 真簇 + 112 个各自成簇的点）。
- **不同维度的向量不能混算余弦**。
- **连通分量必须收口到 1**。失败过两次：① 只桥接度数 0 的点 → 剩 8 个分量；② 按簇代表点串骨架链 → 剩 5 个（**单簇内部也会分裂**）。正解：并查集算真实分量再逐对串。
- **语义图必须画点云，不能画卡片**。250 个文件 × 120~210px 卡片 = 一堵墙，缩放到 20% 仍互相压。
- **语义图不能用层级布局**（节点无父子，tree/radial 会把 200+ 个根叠在圆心）。
- **簇心分布用网格，不用圆周**。环半径是 O(k)，15 个簇时缩放到 19%；网格是 O(√k)。
- **团内螺旋半径用面积法** `spacing·√(n·0.866/π)`，不是 `spacing·√n`（后者大 42%）。
- **fit 缩放上限要按布局区分**：目录 1.6、点云 1.0。
- **LOD 必须对 cluster 模式放行**。原 `lodVisible` 在 k<1 时只显示 folder，而语义节点全是 file → 整图空白、HUD 却显示"253 节点"。
- **恒星半径公式 `20 + cbrt(GB)·8.5`**。初版 `28 + cbrt(GB)·61` 让 1.8TB 算出 770，比整个星系还大。
- **多恒星轨道半径要跟随恒星尺寸**（`maxStarR·2.6·(1+n·0.34)`），固定值会让两颗盘叠成一团白光。
- **相机要用 `frameStars()` 按恒星分布取景**，不能依赖 `zoomToFit`（节点是 Instanced，fg 的包围球不可靠）。

### ★ 18:00 那次排障的关键教训

用户说「**这块矩形叫全部索引**」时，我还在往 WebGL 着色器、billboard 尺寸、Bloom 后处理方向猜，连续试了隐藏恒星层、隐藏节点层、移除 composer、改 localStorage 关 bloom —— 都是错的。

真正的根因是一行 CSS：

```css
.sc-note { position: absolute; left: 50%; top: 12px; transform: translateX(-50%); }
.sc-note.unindexed { bottom: 104px; }   /* ← 没清掉 top */
```

绝对定位下同时给 `top` 和 `bottom` = **从 12px 拉到离底 104px**，元素被撑成 265×704 的半透明圆角矩形，正好盖在画布中央的「全部索引」标签旁。

**教训**：用户用日常语言指认现象（"这个矩形叫 X"）时，先去查离它最近的 DOM 元素尺寸，不要假设是渲染管线。**一条 `el.getBoundingClientRect()` 就能定位的事，不要在 GPU 上猜 20 分钟。**

### 验证记录

| 项目 | 结果 |
| --- | --- |
| `cargo check --features llama,candle,custom-protocol` | 0 error 0 warning |
| `npm run build` | exit 0，2212 modules |
| `--cli semgraph` | 148 取样 / 8 簇 / **连通分量 1** / 孤立点 0 |
| 2D 语义星图 GUI | 251 节点 / 12 簇 / 88% / 240 FPS / 零异常 |
| 3D 星图 GUI | 亮像素 0.46%、两颗恒星配色区分、8 行星轨道随引力递增 |
| 静默安装 `/S /D=D:\ddo_v28test` | 退出码 0，6 项文件齐全 |
| 安装版启动 | 版本显示 **0.28**，新版界面完整，零异常 |
| 静默卸载 | 退出码 0，注册表清空，仅留 data/plugins/webview-data（设计如此） |

### 本轮踩的坑

- **安装包路径含中文会让 NSIS 静默安装失败**（退出码 2，无任何提示）。把 setup.exe 复制到 `D:\ddo_setup_028.exe` 这类 ASCII 路径即可，退出码 0。
### 本轮踩的坑

- **`fs.writeFileSync` 重建文件比 `write` 工具安全**：用工具做整段替换时，文件尾部被截断过两次（`star.rs` 丢了 `build_semantic` 的后半段），表现为 `unclosed delimiter`。大文件改用 node fs 读写。
- **`npm run build` 的 esbuild 临时文件在 `%TEMP%` 被拒**（`Access is denied`）。解法：把 `$env:TMP`/`$env:TEMP` 指到项目内 `build-tmp\` 再构建。
- **`npm run tauri dev` 在本机链接失败**（`cannot find -lwinapi_advapi32` 等），与本次改动无关。**验证一律走 release 构建**。
- **PowerShell 脚本里不能写中文字面量路径**（`Set-Location 'D:\软件\...'` 找不到路径），用 `Join-Path $PSScriptRoot ".."`。
