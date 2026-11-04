# DeepDark_Oracle 版本说明（CHANGELOG）

版本号规则：对外显示 `0.01` 起、每次改动 +0.01；内部语义化版本同步为 `0.0.N`。
本项目由 DiskSeek（Python + PySide6）重构而来，见文末「从 DiskSeek 到 DeepDark_Oracle」。

---


## v0.27 — 2026-11-03

**七项需求收尾：2D 蓝图、1TB 卡顿、星星点不开、盘符恒星、GPU 空转**

本版处理用户提出的 7 项改造。需求原文见 `docs/aegis/work/2026-11-02-StarAndBlueprint/TaskIntent.md`，蓝图规格见 `清晰平面与蓝图风格需求明细表.xlsx`（单表 15 条）。

### 一、2D 星图改成清晰平面蓝图风格（需求①）

按需求表「去光、加框、换底、变直」的口诀重做：

| 项 | 值 | 落点 |
| --- | --- | --- |
| 底色 | `#dcebf5` | `engine.ts:898` |
| 网格 | `#cce0f0`，间距 40px | `engine.ts:898-901` |
| 节点 | 白底 `#ffffff` + `#2c3e50` 3px 描边，140×40 圆角胶囊 | `engine.ts:1018-1055` |
| 连线 | `#5d768f`，直线，文件级虚线 `[5,5]` | `engine.ts:960`、`glRenderer.ts:259` |
| 文字 | Courier New bold 14px `#2c3e50` | `engine.ts:108` `BLUEPRINT_FONT` |
| 面板 | 白底 + 3px 深边 + 12px 圆角 + 硬阴影 `8px 8px 0` | `GalaxyCanvas.vue:450-589` |

配置集中在 `src/galaxy/model.ts:77` 的 `BLUEPRINT_CONFIG`，`BLUEPRINT_FORCE_CANVAS2D = true`（`:114`）强制走 Canvas 2D 路径 —— WebGL 的 `gl.POINTS` 画不出「白底 + 描边」的胶囊。CDP 实测首屏 1564 节点 / 240 FPS。

### 二、1TB 库卡顿（需求②）

四处聚合热点全部换成按层查询：

| 热点 | 改法 | 效果 |
| --- | --- | --- |
| `graph_data` 首屏 | 逐目录 `dir_child_rollup`（每层 N 次全表扫）→ 全库 `GROUP BY dir` 一次 + 内存折叠 | **15.9~18.8 s → 0.69~0.89 s** |
| `tree_level` | `dir_rollup_under` 拉整棵子树 → `dir_recursive_stats` + `dir_child_rollup` + `dir_direct_counts` | — |
| `tree_search` | 全量 `dir_rollup()` 内存过滤 → `dir_name_search` LIKE 粗筛 + 精确二次过滤 | — |
| `star_children` | `expand_dir` 三条全表级查询 → 折叠表复用 | 1177 → 372 ms |

折叠函数 `fold_dir_children` / `dir_lookup_key` 从 `cmd.rs` 迁到 `db.rs`（:1465 / :1527）供 `star.rs` 复用；`db.rs` 另新增 `ix_files_kind_dir`、`ix_files_dir_size`、`ix_files_drive_kind` 三个索引。

### 三、星星点不开、一点就报错（需求④）

根因不在后端，在 `d3-force`：`initializeLinks` 会把 `link.source/target` **就地改写成节点对象引用**，而 `three-forcegraph` 的更新顺序是 `numDimensions()` 先于 `nodes()`。复用同一份 links 数组重跑时，新一轮还在用旧节点表查新节点，查不到就抛出 `node not found`。

控制台实测：`Error: node not found: kind:other@D:`，抛完 HUD/面包屑/标签全部清空。

修法（`engine3d.ts`）：新增 `private graphLinks()`，用 `idOf` 把两端规范回字符串 id 并过滤掉不在 `byId` 里的残边，`pushGraph()` 每次传新副本。CDP 连钻三层无异常。

### 四、盘符恒星与行星轨道（需求⑤⑥）

- `star.rs` `build_drive_level`：`drive_kind_stats` 按盘聚合，产出 `drive:D:` / `drive:E:` 节点。
- `engine3d.ts:540-643` `assignOrbits`：drive 恒星**两两互绕**（`r = 150 + driveCount*46`、`speed = 0.16/sqrt(driveCount)`、奇偶反向）；单盘时 `radius 0 / speed 0` 静止不动 —— 即「一个盘自转、三个盘三体、四个盘四体」。
- kind 行星环 `ring = 2.4 + idx*0.62`；folder/file 卫星环 `1.7 + (idx%8)*0.34`。
- 点开不再爆量：`GalaxyView.vue:215` 的 `dir-limit=24`、`starFileLimit` 上限 24。

### 五、GPU 占用几乎为零（需求⑦）

诊断过程中排除了两个错误假设：

1. **「模型没上 GPU」是错的**。最初用 `nvidia-smi` 每 0.8 s 采样看不到台阶就下了结论。正确方法是 `--loop-ms=200` 原生循环（单次调用要 0.48 s，bash 的 for/sleep 会被自己拖慢）。改用后立刻看到 **+1.7 GiB 台阶**。
2. **「Db 单 Mutex 导致单核串行」是错的**。实测 `TotalProcessorTime` 斜率 1.03~1.25 核（32 核机器）。真元凶是 `ai/hash.rs` 的 `image_vec_from` 对 pixiv 原图（4000×6000）做全尺寸 `to_rgb8()` 复制再遍历两遍。

四处修复：

1. `ai/hash.rs`：先 `resize_exact(64, 64, Triangle)` 再统计直方图与亮度。
2. `index.rs` `run_loop`：`first_image` 不再要求主模型有多模态能力。
3. `index.rs` `process_image_batch`：新增 embedder/store 选择器；CLIP 改批量前向 `ClipModel::image_data_vecs`。
4. `db.rs` `pending_cond` / `kind_scope`：队列按 kind 过滤。原来 `pending_ids("text")` 不看 kind，**47,691 张图片**被排进文本队列白跑。

| 指标 | 修复前 | 修复后 |
| --- | --- | --- |
| 图片索引吞吐（120 s） | 357 条 = 2.98 张/秒 | **1430 条 = 11.9 张/秒** |
| GPU util 均值 | 11.9% | **28.0%** |
| GPU util 峰值 | 100% | 100% |

另外 `ai/ort.rs` 接通了 ONNX Runtime 的 **DirectML 执行提供器**（`detect_ep` / `attach_ep`），CLIP 实测 `EP=DML` 加载成功 —— 在 CUDA 不可用的前提下补上了 GPU 推理能力。

### 六、CUDA 后端暂缓（需求③的遗留部分）

**本版不产出 CUDA 版**，维持 Vulkan（llama.cpp）+ DirectML（ONNX Runtime）。硬阻断在 `src-tauri/build.rs:26-28`：CUDA 构建必须用 MSVC 目标，本机是 MSYS2 UCRT64 + GNU 工具链，`build.rs` 直接 panic。

本机无 MSVC、无 CUDA Toolkit、C 盘只剩 20 GB。将来换到装有 VS 2022 Build Tools + CUDA Toolkit 的机器，直接跑 `scripts/build-release.ps1` 即可，脚本会自动切换 features 并 Stage CUDA 运行库。完整接手步骤见 `HANDOFF.md` 的「v0.27 增量 → CUDA 后端」章。

### 七、修掉一个会让整个进程消失的 BUG

`db.rs` `fold_dir_children` 用 `&dir[..prefix.len()]` 做前缀判断，`prefix.len()` 是**字节数**，目录名含中文时切片落在多字节字符中间 → `byte index N is not a char boundary`。这是 Tauri 命令线程里的 panic，**非 unwinding 直接 abort 整个进程**（日志只有一行 `thread caused non-unwinding panic. aborting.`）。

改用 `dir.get(..prefix.len())` / `dir.get(prefix.len()..).unwrap_or("")`，并用 CDP 实测点开 `D:\软件` → `触手迷宫1.1` 三层中文目录不闪退。

### 八、产物

| 项 | 值 |
| --- | --- |
| 安装包 | `release/DeepDark_Oracle_0.0.27_x64-setup.exe` |
| 构建特性 | `llama,candle,custom-protocol`（无 candle-cuda） |

### 九、清理

根目录删掉 17 个 `_` 前缀临时脚本、2 个 57 MB 的旧 exe、`.tmpbuild/`（417 MB）、`webview-data/`（81 MB）、`dist/`。

---

## v0.26 — 2026-11-01

**3D 星图：把索引铺成星云 + 安装包清掉一堆过时二进制**

本版按 `2D转3D与星图需求明细.xlsx` 的 **33 条需求**（序号 1~33）实现，同时把安装包里用不上的陈旧 DLL 与临时产物全部清掉，包体回到 33 MB。

### 一、3D 星图引擎（新增 `src/galaxy/star/`，2588 行）

- **`engine3d.ts`（1043 行）**：`ForceGraph3D` 只负责力学与相机（`nodeVisibility` 全关），节点绘制交给一块 `InstancedBufferGeometry` —— 位置/颜色/尺寸/透明度/LOD 全走实例属性，**全场景节点 1 次 draw call**、连线 1 次、星尘 1 次。这是需求 #4「千节点不卡」与 #30「实例化渲染」的共同底座。
- **`StarCanvas.vue`（901 行）**：Vue 外壳，管数据加载、钻取历史、HUD、图例过滤、右键菜单与慢帧降级。
- **`nebula.ts`（297 行）**：3000 粒星尘球内均匀分布（`r = R·cbrt(rnd)`），空间哈希就近配对连线（每点至多一条，`maxLen = R*0.07`），`SphereGeometry(R*1.6)` 球壳做背景，`FogExp2` 密度 0.002。
- **`shaders.ts`（174 行）**：glow（512px）/ dot（64px）/ cloud（256px）三张**运行时程序化生成**的贴图 + 节点与粒子着色器。源码里没有任何图片资源。
- **`sketch.ts`（173 行）**：铅绘滤镜 ShaderPass，Sobel 采样深度出线条 + fbm 噪声偏 UV 做手抖 + 噪声等值线做拓扑线 + 程序化纸纹 Multiply。默认关闭（设置 → 图形与 GPU 里开）。

**视觉**（需求 #8~17）：背景 `#050510` + 深色雾效；节点 `ShaderMaterial` + `AdditiveBlending` + 程序化 glow；连线 `0x225588` / `0xaab8ff`，alpha 0.06，悬停升到 0.45；星尘自转 0.08。

### 二、Bloom 参数有意偏离需求表（附实测）

需求 #12 / #17 给的基准是 `strength 1.8 / radius 0.5 / threshold 0.1`。**照抄的实测结果是整屏过曝** —— 亮像素占比 51.2%，画面基本是白的。原因是那套参数的前提是「纯黑背景 + 少量发光体」，而本项目的深空底色 `#050510` 亮度已经贴着 `threshold 0.1`，星云球壳又是整片参与泛光，叠完直接饱和。

改法：`threshold 0.1 → 0.62`、`radius 0.5 → 0.35`、`strength 1.8 → 0.85（高端）/ 0.7 / 0.55`。复测亮像素降到 **4.65%**，观感上的辉光仍在，但只落在真正的发光体上。取舍写在 `engine3d.ts` 的 `applyBloom()` 注释里。

### 三、修掉一个会让星图整页白屏的 BUG

出包后实机启动星图，整页「界面加载出错」，控制台是：

```
Uncaught Error: An error occurred. See https://github.com/styled-components/polished/...
```

定位链：`3d-force-graph` 依赖的 `three-render-objects` 用 `polished` 解析背景色，而 `polished/lib/color/parseToRgb.js` 第一行就是 `if (typeof color !== 'string') throw`。本项目原来给 `backgroundColor()` 传的是数字 `0x050510`，直接抛错把整棵组件树打挂。

**修法**：`engine3d.ts:73-79` 拆成 `BG_PLAIN = 0x080b14`（给 `scene.background` 用，数字没问题）与 `BG_DEEP_CSS = "#050510"`（给 `backgroundColor()` 用，必须是 CSS 串）。这两条路径混用是这类 BUG 的常见来源。

### 四、渐进式探索（需求 #19~23）

- **Level 0**：9 个类型节点（文件夹 / 文档 / 图片 / 代码 / 视频 / 音频 / 压缩包 / 程序 / 文本），由 `star_overview` 聚合。
- **Level 1~3**：`star_children(nodeId)` 逐层下拉，带 `dirLimit` / `fileLimit`；300ms 内的 `star_layout_get/put` 复用坐标，钻取回来位置不跳。
- **导航**：左下角面包屑逐级点击，工具栏 `←`/`→` 走 `history` 栈（`historyAt` 越界置灰）。
- **防连线泛滥**（#22）：连线默认 alpha 0.06 只作视觉引导，悬停时该节点的边升到 0.45；图例条点一下按类型过滤节点。

### 五、安装包清理

删掉的（全部经实测确认不影响运行）：

| 文件 / 目录 | 体积 | 为什么 |
| --- | --- | --- |
| `.gitattributes` | 163 B | 项目不是 git 仓库 |
| `libgcc_s_seh-1.dll` / `libstdc++-6.dll` / `libwinpthread-1.dll` | 2.88 MB | MSYS2 编译期运行时，exe 已静态链接 |
| `onnxruntime.dll` | 13.56 MB | 可选扩展，不该随包分发（用 `install-onnxruntime.ps1` 自己装） |
| `vulkan-1.dll` | 724 KB | 系统 `System32` 自带；移走后实测启动正常 |
| `resources/` `plugins/` | — | 冗余旧副本 / 源码零引用（安装时由 hooks 创建） |
| 92 个临时脚本 / 截图 / 测试目录 | **79.1 MB** | 开发期取证残留 |

**必须保留 `WebView2Loader.dll`（160,880 B）**：实测删掉即 `0xC0000135 STATUS_DLL_NOT_FOUND`（退出码 3221225781），窗口一个都不出。它由 cargo 在 `target/release/` 下生成但不会自动复制到项目根。

根目录从 124 项降到 31 项，`_` 前缀残留 0 个。

### 六、产物

| 项 | 值 |
| --- | --- |
| 安装包 | `release/DeepDark_Oracle_0.0.26_x64-setup.exe`，34,642,746 字节（33.04 MiB） |
| SHA-256 | `a8e945321e1f220c587436285921cd97106c7b0adc114e4c6319eac0815530ea` |
| 免安装 exe | 57,692,160 字节，SHA-256 `edfc792b2e506b34f4bc27c1bee09c5449edf00e05c1703129555a7828226074` |

静默安装 → 启动 → 静默卸载三关实测通过；安装目录 7 文件 + 3 目录（139 MB，含 ffmpeg 87 MB），**不含**模型权重、索引数据、MinGW 运行时 DLL、`vulkan-1.dll`、`onnxruntime.dll`。

### 七、需求表里没有照做的部分

| 条 | 原方案 | 理由 |
| --- | --- | --- |
| #13 SpriteMaterial | 每节点一个 Sprite | 换 Instanced 广告牌，1 draw call 顶掉 N 个 Sprite |
| #31 KTX2 + Texture Atlas | 压缩纹理 / 图集 | 所有贴图运行时程序化生成，磁盘上没有图片资产可压；实际做的是 2 的幂尺寸 + mipmap + 各向异性 |
| #32 Mesh BVH | 空间加速结构做射线检测 | 改 CPU 屏幕投影，比 `Raycaster` 对一万个球做检测快两个数量级 |
| #4 移动端 `controlType=orbit` | 移动端手势 | 桌面程序，无触摸目标 |

其余 28 条全部按需求落地。详细对照见 `docs/DeepDark_Oracle_v0.26_交付说明.md`；需求 #18 的 AI 绘画提示词另出 `docs/星图视觉参考提示词.md`。

---

## v0.25 — 2026-11-01

**GPU 加速兼容性：推理后端探测 + WebGL/Canvas 双渲染管线 + 图形质量面板**

本版按《需求明细汇总表》第三组「GPU 加速兼容性」REQ-093~108 实现。需求原文是按 Python + PyTorch 写的（`torch.cuda.is_available()`、`onnxruntime-directml`、`pip install` 之类），本项目是 Tauri + Rust + llama.cpp，没有 Python 运行时，加速后端在**编译期**就固定了，所以全部按现有架构做等价改写 —— 每一条的对应关系见下文。

### 一、推理侧：GPU 能力探测（REQ-093 / 094 / 096 / 097 / 098~102）

- **新增 `src-tauri/src/ai/gpu.rs`**：进程内唯一的 GPU 能力探测入口，`probe()` **永不返回 Err**（检测失败绝不该让主程序起不来）。
- **后端来源**（REQ-093 / REQ-096）：不是 `torch.cuda.is_available()`，而是遍历 GGML 后端注册表（`ggml_backend_reg_count` / `ggml_backend_reg_get`），用 `ggml_backend_get_features` 拿每个后端编进二进制时启用的特性（`cuda` / `vulkan` / `metal` / `cpu` + 具体能力位）。为此在 `native/llama_shim.cpp` 新增三个 C 导出：`ds_llama_backend_count()` / `ds_llama_backend_info()` / `ds_llama_device_tier()`。
- **显存分档**（REQ-097）：`classify_tier(vram_mb)` → Tier0 ≥8 GiB 全量下放 / Tier1 4~8 GiB 建议量化 / Tier2 1~4 GiB 小模型 / Tier3 <1 GiB 回落 CPU。阈值与 `llama_shim.cpp` 的 `ds_llama_device_tier()` **逐字一致**，否则界面说"显存够用"、实际加载却退回 CPU，用户只会觉得检测结果是骗人的。
- **模型加载真的按分档走**：`llama_shim.cpp` 的 `ds_llama_create()` 里 `mparams.n_gpu_layers` 原来写死 `-1`（无脑全下放），改成：未指定设备 → `0`（纯 CPU）；指定设备 → 先查显存，低于 1 GiB 视为 Tier3 直接退回 CPU，否则 `-1` 全量下放。
- **CUDA Toolkit 校验**（REQ-094）：解析 `nvcc --version` 的 `release X.Y`（回退读 `CUDA_PATH` 下的 `version.txt`）；不匹配时给出**按显卡型号**的版本建议（RTX 40/50 系 → CUDA 12.1+，RTX 30 系 → 11.8+，其余 → 11.8+），而不是"去装一下"这种废话。
- **`--skip-cuda-check` 紧急跳过**：启动参数存在时探照常跑但不做工具包校验，报告里记一条"本次已跳过"。
- **启动即写 `gpu_report.log`**（REQ-093）：数据目录下的 JSON 报告，同时在 stderr 打一行 `GPU: backend=... status=... tier=... — 原因`。整段在后台线程里做（探测会起 `nvidia-smi` 子进程，冷启动时能吃掉几百毫秒，放主线程会让窗口白屏更久）。
- **新命令 `gpu_report` / `gpu_refresh_report`** + **`--cli gpu [--json]`**：命令行下能看到平台、后端、显卡、显存、分档、驱动、CUDA、结论、设备列表、已编入后端。
- **AMD / Intel / Apple 分支**（REQ-098~101）：`classify_vendor()` 按设备名认厂商；AMD 额外探 `rocm-smi`，Intel 探 oneAPI/OpenVINO 安装路径，Apple 说明走 Metal。Windows 下 AMD 明确提示优先 Vulkan 或 DirectML。
- **ONNX Runtime**（REQ-102）：`ort.rs` 仍是纯 CPU FFI 绑定（不引第三方 crate），本版把 `ORT_PROVIDER_ORDER`（CUDA → DirectML → CPU）固化下来并在报告里如实展示"装没装扩展 / 会按什么顺序协商"，**不假装装了扩展就一定跑 GPU** —— 普通 CPU 版 onnxruntime.dll 根本不会提供 DirectMLExecutionProvider。

### 二、渲染侧：WebGL + Canvas 2D 双管线（REQ-103 / 104 / 105 / 106）

- **新增 `src/galaxy/capability.ts`**：`detectWebGL()` 依次试 `webgl2` → `webgl` → 无，读 `UNMASKED_VENDOR_WEBGL` / `UNMASKED_RENDERER_WEBGL` / `MAX_TEXTURE_SIZE` / `MAX_VERTEX_ATTRIBS` / `EXT_float_blend` / `getShaderPrecisionFormat`，探测完**立刻 `loseContext()` 归还**（Chromium 系同时存活的上下文有硬上限）。`webglSupport()` 带缓存，一次会话只真探一次。
- **新增 `src/galaxy/glRenderer.ts`**（380 行）：自绘 WebGL 点精灵。节点 = 1 顶点（位置/颜色/半径走顶点属性，圆形靠 fragment shader 里 `discard`），连线 = 2 顶点 `gl.LINES`，相机只改 uniform，CPU 侧不重算坐标。**没有引 PixiJS** —— 这里是离线桌面程序，而且图谱只需要"圆 + 线"两种图元，自绘 400KB 依赖不值当，也更容易在驱动异常时精确回退。
- **着色器兼容降级**（REQ-104）：先试 `highp` 高性能版，编译失败自动换 `mediump` 兼容版；两个都失败就抛错让上层回退 Canvas 2D。`compile()` 刻意**不抛异常**，返回 null 交给调用方决定降级策略。
- **已知驱动问题库**（REQ-105）：`KNOWN_DRIVER_BUGS` 里 4 条（老 Intel HD → 强制 Canvas 2D、AMD Vega/R 系 → 兼容着色器、SwiftShader/软件渲染 → 强制 Canvas 2D、NVIDIA → 建议关后处理）。匹配刻意保守：宁可少命中让用户手动切画质，也不要误判把正常显卡降级 —— 降级后帧率掉得明显，用户只会觉得"这软件越更新越卡"。
- **DPI 自适应**（REQ-106）：`effectiveDpr(profile, backend, deviceRatio, override)`，Canvas 2D 路径压到 `min(2, 档位上限)`（圆本来就看不出锯齿，3 倍像素是纯浪费），WebGL 路径才给到档位上限。设置页可手动选 1x/2x/3x，**手动档位仍受画质上限约束**（低画质选 3x 只会更卡）。
- **双管线切换**：`GalaxyEngine` 构造时按探测结果 + 用户偏好选后端；WebGL 初始化失败或绘制期抛错（驱动重置/休眠唤醒）自动回退 Canvas 2D 并记录原因，画布上方弹一条黄色提示条说明为什么降级。**一块 canvas 一旦绑定过某种上下文就不能再换**，所以切换后端时 `GalaxyCanvas` 用 `:key` 强制换一块新 DOM 画布重建引擎。
- **鱼眼透镜在 GL 下也有**（REQ-077）：鱼眼位移做进顶点着色器（`u_fish` = 开关/鼠标位置/半径，`u_fishScale` = 放大倍数），几千节点的推开由 GPU 算，CPU 零成本。
- **慢帧自动提示**（REQ-103 的"renderTime > 16ms 提示降级"）：连续 45 帧超过 16.7ms 才提示一次，切画质会重置计数（否则一次卡顿会永久压制后续提示）。

### 三、设置页「图形与 GPU」（REQ-107 / 108）

- **新增 `src/components/SettingsGpuTab.vue`**，设置抽屉第七个标签页，五张卡：GPU 能力报告（显卡/后端/显存条/驱动/CUDA/分档/ONNX EP）→ 画质档位（低/中/高/极高）→ 渲染后端与分辨率（自动/强制 WebGL/强制 Canvas 2D + 1x~3x）→ 渲染能力（探测到的渲染器/着色器精度/最大纹理/顶点属性/浮点混合/像素比 + 命中的驱动补丁）→ 兼容性诊断与崩溃记录。
- **画质四档**（REQ-108）：`QUALITY_PROFILES` 控制光晕、贝塞尔连线、背景星点、`maxDpr`、节点上限（5000 / 12000 / 40000 / 120000）。
- **崩溃留证**（REQ-108）：新增 `src/galaxy/telemetry.ts`。离线程序没有服务器可上报，做的是"本地留证"——记录 `context-lost` / `shader-fail` / `init-fail` / `slow-frame` 四类事件，设置页可直接复制给开发者。**隐私**：只记显卡/驱动/后端这类渲染元信息，**绝不记录文件名、路径或任何文件内容**（需求表点名要求）。
- **新增 `scripts/gpu-check.ps1`**（REQ-107 的等价实现）：需求表原方案是 GitHub Actions 矩阵跑 chrome/firefox/safari/edge + 模拟 GPU 降级，本项目没有 CI 也没有浏览器矩阵，改成「本机自检 + 基线对比 + HTML 报告」：跑 `--cli gpu --json` → 归一化状态 → 打印摘要 → 给 `-Baseline` 时比对显存档位退化 / 后端变化 / 可用显存下降 >20% → 输出 `docs/gpu-report/gpu-report-<时间戳>.json` 与同名 `.html`；`-Strict` 模式下退化或"不支持"返回 1，可直接挂到发布前检查。

### 四、Vulkan 后端默认启用

- 之前必须手动设 `DEEPAISEE_VULKAN=1` + `VULKAN_SDK` 才编得进 Vulkan 后端，现在 **`build.rs` 自动探测**：依次找 `$VULKAN_SDK` / `$DEEPAISEE_VULKAN_ROOT` / `$MSYS2_ROOT` / 仓库内 `.tools/vulkan{,-sdk}` / `D:\dsh-tools\msys64\ucrt64` / `C:\msys64\ucrt64`，要求 `include/vulkan/vulkan.h` 与 `bin/glslc.exe` 同时存在。`scripts/activate-local.ps1` 同步做了探测与提示。
- **`scripts/build-release.ps1` 删掉了两行重复的 `DEEPAISEE_VULKAN = "0"`** —— 那是早期为了出包稳定加的强制关闭，一直没删，导致自动探测形同虚设，装好的 Vulkan SDK 白放着不用，GPU 推理只能退回 CPU。
- 本机实测：`CMakeCache.txt` 里 `DEEPAISEE_VULKAN:BOOL=ON`，NVIDIA RTX 4060 Laptop（8188 MiB）被正确识别为 `vulkan` 后端 + Tier 0。
- 体积代价：exe 从 v0.24 的 23.9 MB 涨到 **57.3 MB**（Vulkan 后端 + 静态链接的 C++ 运行时），安装包从 30,501,454 字节（29.09 MiB）涨到 **34,276,337 字节（32.7 MB）**。仍然不含任何模型权重、索引数据或可选扩展。

### 五、需求表里**没有**照做的部分（附理由）

| 需求 | 原方案 | 本版做法 / 理由 |
| --- | --- | --- |
| REQ-095 | `pip uninstall/install` 重装 GPU 版 PyTorch | 本项目无 PyTorch，等价改写为「该装哪个 CUDA Toolkit 才能编出带 CUDA 后端的版本」，按显卡型号给建议 |
| REQ-102 | 装 `onnxruntime-directml` + provider 自动协商 | 只做 provider 顺序常量 + 如实展示；`ort.rs` 仍是纯 CPU FFI，跨模态扩展是可选安装项 |
| REQ-107 | GitHub Actions 多浏览器 × 多 GPU 矩阵 | 无 CI、无浏览器矩阵，改为本机自检 + 基线回归 + HTML 报告脚本 |
| REQ-105 | 在线驱动 Bug 数据库 JSON 远程更新 | 离线程序不联网，驱动库内置在 `capability.ts` 里，跟着版本走 |

---
## v0.24 — 2026-10-31

**全局索引星系图谱 + 八种思维模型图 + 安装包瘦身**

本版按《DeepDark_Oracle_需求明细汇总表》实现「双模态可视化引擎」与「思维模型图」两条主线，同时把安装包里用不上的陈旧 DLL 彻底清掉。

### 一、全局索引星系图谱（`src/galaxy/`）

- **新的画布引擎**：`src/galaxy/engine.ts`（789 行）用 `<canvas>` + `d3-force` 自己实现力导向渲染，而不是用 D3 的 SVG 选择器——按需求 REQ-032~041「禁用 DOM 节点」的要求，一万节点滚动缩放也不掉帧。
- **节点视觉**：半径 `r = 8 + 3*log(childCount+1)`（REQ-006）；配色严格按需求表 —— 文件夹 `#4E79A7`、文档 `#59A14F`、图片 `#B07AA1`、代码 `#F28E2B`、视频 `#E15759`、其他 `#9C755F`（REQ-008）。
- **三级 LOD**（REQ-034）：缩放 <0.3x 只画文件夹、0.3~1x 画文件夹 + 重要文件（半径 ≥9）、>1x 才画全部；标签按屏幕像素 ≥15px 才绘制（REQ-010），远景不糊成一团。
- **视口裁剪**：只绘制视口外扩 50px 内的节点与连线，10 万文件量级下平移依然跟手。
- **连线**：contains 关系用二次贝塞尔曲线，透明度 0.3~0.5、线宽 1~2px（REQ-011），父子方向靠曲率表达。
- **交互**（REQ-016~031）：滚轮以鼠标位置为锚点缩放（0.1x~5x）、拖拽带 0.85 阻尼、双击展开/折叠子节点、右键菜单（打开文件 / 在文件夹中显示 / 聚焦 / 展开折叠 / 以此为根 / 复位）、相机缓动 260ms。
- **鱼眼透镜**（REQ-077）：半径 150px、中心 1.5x 放大，鼠标附近的节点被推开，工具栏一键开关。
- **捏合建边**（REQ-074）：Ctrl + 拖拽从一个节点连到另一个节点，松开预览一条黄色虚线关系边。

### 二、四个入口

- **顶栏星系图按钮**（`Network` 图标）进入全屏 `GalaxyView`：面包屑 + 「层数 / 每目录文件数」两个下拉 + 「全部根 / 聚焦选中」。
- **快捷键**：`Ctrl+G` 在搜索页与星系图之间切换，`Ctrl+F` 从星系图直接回搜索并聚焦输入框。
- **搜索结果页视图切换**：结果工具栏右侧多了「列表 / 图谱 / 思维图」三档，切到「图谱」就把当前这批命中结果渲染成星系图，**不用跳页面就能看这批结果的簇结构**。
- **索引面板双 Tab**：右上角「N 个文件 · X GB」弹出的面板顶部新增「目录树 / 星系图」两个 Tab，星系图 Tab 里是紧凑版画布，跟随当前选中的目录。

### 三、八种思维模型图（`src/components/MindMap.vue`）

按 REQ-053~061 实现，全部用 SVG 绘制、支持选中与详情卡：

| 模型 | 图形 | 说明 |
| --- | --- | --- |
| 界定图 definition | 嵌套圆 packing | 按文件类型分层，圆面积 = 体积 |
| 描述图 description | 中心辐射 | 中心是检索词，周围是所在目录 |
| 比较图 comparison | 韦恩图 | 三种主要类型的两两交集 |
| 分类图 classification | 树状 dendrogram | 按扩展名逐级展开 |
| 拆解图 decomposition | 旭日图 sunburst | 类型 → 扩展名两级下钻 |
| 顺序图 sequence | 时间线 | 按修改月份排布，带时间轴 |
| 因果图 causality | 桑基图 | 目录 → 类型的流向与权重 |
| 类比图 analogy | 平行对照 | 类型 / 扩展名两组对照 |

右下角选中条目详情卡支持直接打开文件或在文件夹中显示。

### 四、后端新增命令

- **`graph_data(root?, depth?, max_nodes?, file_limit?)`**（`src-tauri/src/cmd.rs:1823`）：返回 `GraphData { root, name, nodes, edges, total_files, total_bytes, truncated }`。数据全部来自 SQLite 的 `dir_rollup_under()` 前缀聚合，**不触发任何磁盘遍历**；逐层按递归文件数降序展开，超过 `max_nodes`（默认 900，上限 8000）时截断并置 `truncated`。
  - 根路径大小写用库里的真实路径回填（`dir_canon` 映射），否则第 0 层 `WHERE dir = ?1`（区分大小写）查不到直属文件。
  - 子目录归并从「每层全表扫描」改成预建 `child_index` / `dir_canon` 两张哈希表，复杂度从 O(目录数²) 降到 O(目录数)。
- **`mind_map_data(model, paths, query?)`**（`src-tauri/src/cmd.rs:2149`）：把一批文件按指定思维模型分组，返回 `MindMapOut { model, title, subtitle, groups, total_files, total_bytes, buckets }`；不传 paths 时取最近 400 个已索引文件，每组上限 120 条。

### 五、安装包瘦身与链接方式

- **删掉根目录的历史遗留文件**：`.gitattributes`、`DeepDark_Oracle.exe`（旧构建）、`DeepDark_Oracle 启动器.cmd`、`launcher.ps1`、`WebView2Loader.dll`、`libgcc_s_seh-1.dll`、`libstdc++-6.dll`、`libwinpthread-1.dll`、`onnxruntime.dll`，以及 `tools/` 下的 msys2 安装器(85MB) / node zip(34.9MB) / rustup-init(12.7MB) 等下载残留 —— 合计约 170 MB，另外单独删掉了 682 MB 的 v0.22 旧安装包。
- **`tauri.conf.json` 与 `scripts/build-release.ps1` 里对应的 resources 条目同步删除**，`installer-hooks.nsh` 只保留 `tools/bin/ffmpeg.exe` 的说明。
- **静态链入 GNU 运行时**（`src-tauri/build.rs:244-262`）：改为 `-static-libgcc -static-libstdc++` 并静态链 `winpthread`，exe 不再依赖 `libstdc++-6.dll` / `libgcc_s_seh-1.dll` / `libwinpthread-1.dll` 三个 MinGW DLL——少拷任何一个都会让程序双击后弹「系统错误」直接退出（0xC0000135）。
- **`WebView2Loader.dll` 仍随包分发**：`webview2-com-sys 0.39` 的宏写死了「只有 MSVC 才用 `WebView2LoaderStatic` 静态库，GNU 目标必须动态链 DLL」。这个 DLL 是构建时从 crate 现取的（160,880B），和根目录那份 2021 年的陈旧副本不是一回事。
- **安装包不含模型与索引**：`models/`（embeddinggemma2 GGUF 558 MB）与 `data/`（index.db + 缩略图 + 向量）都不进包，成品 **30,501,454 字节（29.09 MiB）**。

### 六、构建工具链

- **GNU 工具链不认非 ASCII 路径**：`scripts/activate-local.ps1` 现在自动把 `RUSTUP_HOME` 镜像到纯 ASCII 盘（优先复用已存在的 `D:\ddrust`，否则 `robocopy` 到 `D:\DeepDark_Oracle-toolchain\rustup`），并把 `CARGO_TARGET_DIR` 指到 `D:\DeepDark_Oracle-build\target`。**sysroot 和 target 目录都必须是 ASCII**，否则 `ld.exe` 把中文按 GBK 解析成乱码，报 `cannot find ...libcore-....rlib`。
- **PATH 顺序**：MSYS2 UCRT64 必须排在 Rust toolchain 之前，否则 gcc 会加载错版本的 `libgcc_s_seh-1.dll` 而静默失败（表现为 `libsqlite3-sys` 报 `Compiler family detection failed` 且 stderr 为空）。
- **MSYS2 自动探测**：依次找 `tools\msys64`、`$env:MSYS2_ROOT`、`D:\dsh-tools\msys64`、`C:\msys64` 等位置，找不到才回退。
- **`patch-llama-embeddinggemma2.ps1` 判定失效**：脚本原先用 `build_inp_embd(model.tok_embd);` 判断上游是否已支持 gemma-embedding2，但新版 llama.cpp 把该调用改成了 `build_inp_embd(model.tok_embd, sqrtf(n_embd));`，条件恒假导致走进 `git apply` 分支（本项目不是 git 仓库）而报错。改为前缀匹配后恢复正常。
- **llama_shim 跟进 llama.cpp 0.7 的 mtmd API**：`mtmd_context_params.backend_device`（字符串）改成 `device`（`ggml_backend_dev_t` 句柄）；`mtmd_set_image_max_tokens()` 在新版已被移除，图像 token 预算改为只在 `mtmd_context_params.image_max_tokens` 初始化时设定。

### 七、清理

- 删除 `release/` 下 682 MB 的 v0.22 旧安装包与本轮开发过程中的临时脚本 / 日志。
- `src-tauri/target/` 的 debug 产物不再参与打包。

---
