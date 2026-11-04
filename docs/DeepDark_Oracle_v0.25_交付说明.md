# DeepDark_Oracle v0.25 交付说明

生成时间：2026-11-01
交付物：`release/DeepDark_Oracle_0.0.25_x64-setup.exe`（34,276,337 字节 / 32.7 MB）
SHA-256：`677aca3e6367cb247e3ea003750081a686c62bbf47a2a06dee7961501912d606`

---

## 一、这一版解决什么

这次的需求来源是 `DeepDark_Oracle_GPU兼容性需求明细表.xlsx` 的第三组「GPU 加速兼容性」，**REQ-093 ~ REQ-108 共 16 条**。

需求表是按 **Python + PyTorch** 的世界写的：`torch.cuda.is_available()`、`pip install onnxruntime-directml`、`torch_directml`、`torch.xpu`。本项目是 **Tauri + Rust + llama.cpp**，没有 Python 运行时，加速后端在**编译期**就定死了。所以这一版的工作不是照抄 Python API，而是把每一条需求翻译成本架构下的等价能力。

| 需求分组 | 编号 | 本版对应做法 |
| --- | --- | --- |
| 环境诊断 | REQ-093 / 094 / 095 | 遍历 GGML 后端注册表 + `nvcc --version` 解析 + 按显卡型号给建议版本 |
| 设备选择与分档 | REQ-096 / 097 | `classify_tier()` 四档显存，`ds_llama_create` 真的按档位设 `n_gpu_layers` |
| 厂商适配 | REQ-098 / 099 / 100 / 101 | AMD / Intel / Apple 走探测 + 报告里的后端清单，判定口径写在 `ai/gpu.rs` |
| 推理引擎 | REQ-102 | ONNX Runtime 存在性与 EP 顺序展示（不自带、不打包） |
| 渲染兼容 | REQ-103 / 104 / 105 / 106 | WebGL 2 → WebGL 1 → Canvas 2D 三级降级 + 着色器精度回退 + 驱动问题库 + DPI 自适应 |
| 测试与兜底 | REQ-107 / 108 | `scripts/gpu-check.ps1` 本机基线比对；设置页「图形与 GPU」五张卡 + 崩溃留证 |

顺带完成的两件交付动作：**Vulkan 后端纳入默认构建**（此前必须手动设环境变量），以及**修掉两个会让页面白屏的前端缺陷**（详见第六节）。

---

## 二、设置 → 图形与 GPU（主入口）

`src/components/SettingsGpuTab.vue`（342 行）。左侧导航在「局域网配置」和「可选扩展」之间新增一项。五张卡：

| 卡片 | 给用户看什么 | 对应需求 |
| --- | --- | --- |
| GPU 能力报告 | 显卡型号 / 推理后端 / 显存占用条 / 驱动版本 / CUDA 工具包（没装会给出按型号的建议版本）/ 显存分档 / ONNX EP | REQ-093 / 094 / 097 / 102 |
| 画质档位 | 低 / 中 / 高 / 极高，控制光晕、贝塞尔连线、背景星点、DPR 上限、节点上限 | REQ-108 |
| 渲染后端 | 自动检测 / 强制 WebGL / 强制 Canvas 2D；渲染分辨率 自动 / 1x / 2x / 3x | REQ-103 / 106 |
| 渲染能力 | 渲染器串、着色器精度、最大纹理、顶点属性、浮点混合、屏幕像素比、命中的驱动补丁 | REQ-104 / 105 |
| 兼容性诊断 | 上下文丢失 / 着色器失败 / 初始化失败 / 连续掉帧四类事件，可复制、可清空 | REQ-105 / 108 |

再加一张「已编入二进制文件的后端」表格，列出 `vulkan` / `cpu` 各自的设备数与编译特性。**这一张是判断「加速到底有没有编进去」的唯一可信来源**——它不是读配置文件，是遍历 GGML 后端注册表拿到的运行时事实。

本机（RTX 4060 Laptop）实测：

| 项 | 值 |
| --- | --- |
| 推理后端 | `vulkan`（状态 ready） |
| 显存 | 7957 MiB 总量 / 7189 MiB 空闲 |
| 显存分档 | Tier 1（4~8 GiB） |
| CUDA 工具包 | 未安装（本版不需要） |
| 驱动 | 566.26 |
| 已编入后端 | `Vulkan`（1 设备，加速器）、`CPU`（纯 CPU） |

---

## 三、推理侧：GPU 后端探测

### 探测不看配置文件，看运行时注册表

```rust
// src-tauri/native/llama_shim.cpp 新增三个 C 导出
int ds_llama_backend_count();
int ds_llama_backend_info(int idx, char *name, int name_len, char *features, int features_len,
                         int *device_count, int *has_accelerator);
int ds_llama_device_tier(const char *device_key);
```

`ds_llama_backend_info` 内部走 `ggml_backend_reg_get_proc_address(reg, "ggml_backend_get_features")`，把 `ggml_backend_feature{name, value}` 数组拼成 `k=v;k=v` 返回；加速器判定看该后端是否至少注册了一个 GPU/IGPU 设备。Rust 侧 `src-tauri/src/ai/llama.rs` 的 `gpu_backends()` / `device_tier()` 只是薄包装。

**纯 CPU 后端也会出现在列表里**：如果枚举失败返回空列表，界面就没法区分「只编了 CPU」和「探测本身坏了」。

### 显存分档真的影响模型加载

| 档位 | 显存 | `llama_shim.cpp` 的行为 |
| --- | --- | --- |
| Tier 0 | ≥ 8 GiB | `n_gpu_layers = -1`，全量下放 |
| Tier 1 | 4 ~ 8 GiB | 同上，报告里建议用量化模型 |
| Tier 2 | 1 ~ 4 GiB | 同上，报告里建议小模型或部分下放 |
| Tier 3 | < 1 GiB | `selected_device = nullptr`、`n_gpu_layers = 0`，直接回落 CPU |

未指定设备时 `n_gpu_layers = 0`（纯 CPU）——**默认不赌用户有显卡**。阈值 `1073741824` 字节在 C++ 与 Rust 两侧逐字一致，不会出现「报告说 Tier 3、加载却下放 GPU」这种自相矛盾。

### CUDA 校验

`detect_cuda_toolkit()` 解析 `nvcc --version` 输出里的 `release (\d+\.\d+)`，拿不到再退 `CUDA_PATH` / `CUDA_HOME` 下的 `version.txt`。

- 本项目用 **GNU 工具链**，`build.rs` 里 `cuda_enabled && windows_gnu` 会直接 panic，所以 CUDA 根本编不进来。缺 CUDA 时**状态仍是 ready**（走 Vulkan），只在 `notes` 里写清楚，不会误报成故障。
- `recommend_cuda(gpu_name)` 按型号给建议版本（40 系 → 12.1+，30 系 → 11.8+，20 系/GTX10 → 11.3+），显示在报告的 CUDA 一行里。
- `--skip-cuda-check` 启动参数跳过这一步（需求表的紧急逃生门）。

### 报告落盘

启动后后台线程立刻写一次 `gpu_report.log`（内容是 pretty JSON，不是自由文本），路径在数据目录。设置页「写入诊断日志」会重跑一次并回显路径。后台线程执行，不拖慢首屏。

### 命令行

```powershell
DeepDark_Oracle.exe --cli gpu          # 人读格式
DeepDark_Oracle.exe --cli gpu --json   # 给脚本读
```

---

## 四、渲染侧：WebGL + Canvas 2D 双管线

### 文件分工

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `src/galaxy/capability.ts` | 294 | WebGL 探测、驱动问题库、四档画质预设、`effectiveDpr()`、`pickBackend()` |
| `src/galaxy/glRenderer.ts` | 379 | 自绘 WebGL 点精灵：节点 1 顶点、连线 2 顶点 |
| `src/galaxy/engine.ts` | — | 双管线分发、三级 LOD、视口裁剪、相机、命中测试 |
| `src/galaxy/renderSettings.ts` | — | 画质 / 后端 / DPR 三项偏好，localStorage 持久化 |
| `src/galaxy/telemetry.ts` | 115 | 渲染异常留证 |

### 为什么不用 PixiJS

需求表 REQ-103 点名 PixiJS。实际没引入：这是**离线桌面程序**，装依赖要能离线完成；而且图谱只需要「圆 + 线」两种图元，自绘 WebGL2 点精灵比引入 400 KB 通用框架更可控——尤其是驱动异常时要精确知道在哪一步降级。文件头的注释里写了这个取舍。

### 降级链

```
WebGL 2  ──shader 编译失败──▶  WebGL 1  ──初始化/绘制抛错──▶  Canvas 2D
   │                              │
   └──命中已知驱动问题────────────┴──▶  Canvas 2D（自动）
```

- 每一跳都有理由，并在画布上方显示一条黄色提示说明**为什么**降级，不让用户觉得「怎么变卡了」却查不到原因。
- 用户手动「强制 WebGL」时，`force-canvas` 类驱动问题不再生效（尊重用户一次）。
- 绘制期抛错（驱动重置、休眠唤醒）也会捕获并降级，不是只在启动时检查一次。

### 着色器精度回退（REQ-104）

highp 版本编译失败自动换 mediump 版本重编；两个都失败才降到 Canvas 2D。着色器特性按探测结果生成：`useParticles` / `useBloom` / `useDepthOfField` 三个标志控制后处理。

### 驱动问题库（REQ-105）

`KNOWN_DRIVER_BUGS` 四条，判断条件刻意写保守——**宁可漏判让用户手动切画质，也不要误判把正常显卡强制降级**（降级后帧率掉得明显，用户只会觉得越更新越卡）：

| id | 命中条件 | 动作 |
| --- | --- | --- |
| `intel-hd-old` | Intel + HD 4xxx/5xxx | 强制 Canvas 2D |
| `amd-radeon-vega-glitch` | AMD + Vega / R7 / R9 | 兼容着色器 |
| `swiftshader` | 软件渲染器（无真显卡） | 强制 Canvas 2D |
| `nvidia-old-postprocess` | NVIDIA | 建议切低画质（不强制） |

### DPI 自适应（REQ-106）

`effectiveDpr()` 先按画质档位取上限（低 1x / 中 2x / 高 2x / 极高 3x），Canvas 2D 再压到 2x 以内；**用户手动指定倍数也受该上限约束**——低画质下选 3x 不是「更清楚」，是「更卡」。

### 慢帧提示

连续 45 帧单帧耗时 > 16.7ms 且未提示过，回调一次建议降档，同时记一条 `slow-frame` 到崩溃日志。**只提示一次**，不然掉帧时会刷屏。

### 鱼眼透镜进了顶点着色器

v0.24 的鱼眼是 Canvas 2D 每帧算位移。这一版在 GL 顶点阶段做（`u_fish` / `u_fishScale` 两个 uniform），不用回读像素，也不额外画一遍。

---

## 五、Vulkan 后端纳入默认构建

此前 Vulkan 必须手动 `DEEPAISEE_VULKAN=1` + 设 `VULKAN_SDK` 才能编进去，等于「用户拿到包也永远用不上显卡」。这一版改成**能自动找到就自动开**：

```
候选：$VULKAN_SDK → $DEEPAISEE_VULKAN_ROOT → $MSYS2_ROOT
      → <repo>/.tools/vulkan → <repo>/.tools/vulkan-sdk
      → D:\dsh-tools\msys64\ucrt64 → C:\msys64\ucrt64 → D:\msys64\ucrt64
判定：include|Include/vulkan/vulkan.h 且 bin|Bin/glslc.exe 都在
```

- `SPIRV-Headers` 的 cmake config 三种布局都认（`Lib|share|lib/cmake/SPIRV-Headers`）：Khronos 官方 SDK 放 `Lib/`，MSYS2 放 `share/` 和 `lib/`。
- `scripts/activate-local.ps1` 同步加了 `Test-VulkanRoot()`，命中后设 `VULKAN_SDK` / `DEEPAISEE_VULKAN_ROOT` 并把 `glslc` 目录加进 PATH，构建时打印 `Vulkan SDK: <路径>`。
- **`scripts/build-release.ps1` 开头原来有两行重复的 `if (-not $env:DEEPAISEE_VULKAN) { $env:DEEPAISEE_VULKAN = "0" }`**，会把自动探测永远关掉，已删除。要强制关就自己显式设 `DEEPAISEE_VULKAN=0`。
- 实测 `CMakeCache.txt` 里 `DEEPAISEE_VULKAN:BOOL=ON`，exe 从 23.9 MB 涨到 57.3 MB（Vulkan 后端 + 静态链接的 C++ 运行时），安装包从 29.09 MiB 涨到 32.7 MB。

---

## 六、顺手修掉的两个白屏缺陷

两个都是 **`vite build` 能过、运行时才炸**的类型——项目没装 typescript，没有 `tsc` / `vue-tsc` 挡这一层。

### 1. `SettingsGpuTab.vue` 的 `devicePixelRatio`

模板里写 `{{ devicePixelRatio.toFixed(2) }}`，Vue 编译成 `_ctx.devicePixelRatio`——组件实例上没有这个属性，取到 `undefined` 后调 `.toFixed()` 直接抛错，**整个设置页一片空白**。已改成 `screenDpr` computed（从 `window.devicePixelRatio` 取）。

### 2. `Treemap.vue` 缺 `import { t }`

模板里用了 `t("个文件")` 但脚本没导入 i18n 的 `t`，同样编译成 `_ctx.t`，渲染到那一行就抛错。已补导入。

——这两类问题可以用静态扫描提前发现：用 `@vue/compiler-sfc` 的 `compileScript` 拿 `bindingMetadata`，再 `compileTemplate({ compilerOptions: { bindingMetadata, prefixIdentifiers: true } })`，grep 生成代码里的 `_ctx.xxx` 就是模板自由变量。本轮已用它扫过全部 23 个 `.vue` 文件，现在 problems = 0。

---

## 七、验证记录

| 项目 | 结果 |
| --- | --- |
| `vite build` | 1833 modules，`index-*.js` 341.79 kB / `index-*.css` 89.72 kB |
| 模板自由变量扫描 | 23 个 `.vue`，problems 0 |
| `cargo check --lib` | 0 error 0 warning |
| `npm run tauri:release` | 一条命令出包，EXIT=0 |
| exe 体积 | 57,298,944 字节 |
| 安装包 | 34,276,337 字节（32.7 MB），13 个文件 |
| exe 依赖（objdump） | 系统 DLL + `WebView2Loader.dll` + `vulkan-1.dll`；`libstdc++-6.dll` / `libgcc_s_seh-1.dll` / `libwinpthread-1.dll` 全无 |
| `--cli gpu` | 后端 `vulkan` / ready；7957 MiB、Tier 1；驱动 566.26 |
| 安装包内容 | 不含 models 权重、data 索引、`onnxruntime.dll`、三个 MinGW 运行时 DLL |
| 静默安装 | `setup.exe /currentuser /S` → 装到 `D:\ddorealtest`，`DeepDark_Oracle.exe` / `WebView2Loader.dll` / `tools\bin\ffmpeg.exe` / 两个 resources 脚本 / `uninstall.exe` 全部就位；启动后主窗口标题正常 |
| 静默卸载 | `uninstall.exe /S /currentuser` → exe、`WebView2Loader.dll`、`uninstall.exe`、`resources\`、`tools\` 全部删除，`HKCU\...\Uninstall\DeepDark_Oracle` 注册表项清空；`data\` `models\` `webview-data\` 按设计保留（用户索引与模型不该被卸载器删掉） |

### 关于 `vulkan-1.dll`

exe 只静态导入 4 个 Vulkan 符号（`vkCmdCopyBuffer` / `vkGetDeviceProcAddr` / `vkGetInstanceProcAddr` / `vkGetPhysicalDeviceFeatures2`），其余全走 `vkGetInstanceProcAddr` 动态取。实测把 `vulkan-1.dll` 从 exe 旁边移走，`--cli gpu` 仍然成功——因为 `C:\Windows\System32\vulkan-1.dll` 由显卡驱动提供。**不打进安装包。**（MSYS2 那份 724 KB，额外依赖 `api-ms-win-crt-*`，本来也不适合分发。）

---

## 八、没照做的四条

| 需求 | 原方案 | 本版做法与理由 |
| --- | --- | --- |
| REQ-095 GPU 版 PyTorch 自动重装引导 | 检测 `cu118`/`cu121` 后缀、生成 `pip uninstall torch` 命令 | 本项目没有 PyTorch 可重装。改成：CUDA 缺失时按显卡型号给建议版本 + 指向 Vulkan 后端（`recommend_cuda()`） |
| REQ-102 ONNX Runtime 统一推理引擎 | `export_to_onnx()` + `ONNXRuntimeSession` + 与 PyTorch 对比误差 <1e-3 | ONNX Runtime 在本项目是**可选扩展**，安装包默认不带（这是既定约束）。本版只做**存在性探测与 EP 顺序展示**（CUDA → DirectML → CPU），不冒充已经接入推理 |
| REQ-107 多 GPU 自动化兼容性测试 | GitHub Actions 矩阵 × 四浏览器 + GPU 模拟 | 项目不是 git 仓库、也没有 CI。改成 `scripts/gpu-check.ps1`：本机自检 + 基线比对 + HTML 报告，可真挂到发布前检查（`-Strict` 退化返回 1） |
| REQ-105 在线 Bug 库 | 远程 JSON 热更新 | 离线程序不做远程拉取。问题库内置在 `capability.ts` 的 `KNOWN_DRIVER_BUGS` |

另外两条需求本版只做到一半，说明如下：

- **REQ-099 AMD ROCm（Linux）**：`probe_rocm()` 会查 `rocm-smi` / `/opt/rocm` / `$ROCM_PATH`，但本项目只在 Windows 上构建与验证，**未在 Linux + ROCm 实机跑过**，报告里如实标注为「探测到组件」而非「可用」。
- **REQ-100 Intel XPU**：同理，`probe_intel_toolkit()` 只判断 oneAPI/OpenVINO 组件是否存在。GGML 侧没有对应后端时不会假装能加速。

---

## 九、怎么用

1. 双击 `DeepDark_Oracle_0.0.25_x64-setup.exe` 安装（默认装到非系统盘，装完可改）。
2. 打开程序 → 右上角齿轮 → 左侧「图形与 GPU」：先看「GPU 能力报告」的脸色，再看「已编入二进制文件的后端」确认 Vulkan 在不在。
3. 图谱卡顿就调「画质档位」；花屏/闪烁就切「强制 Canvas 2D」；高分屏字糊就调「渲染分辨率」。
4. 命令行确认：`DeepDark_Oracle.exe --cli gpu`。

---

## 十、构建方式（给你自己用）

```powershell
cd "D:\软件\DeepDark_Oracle"
npm run tauri:release
```

脚本会自己处理：非 ASCII 路径 → 把 Rust toolchain 复制到 ASCII 目录、把 `CARGO_TARGET_DIR` 指到 `D:\DeepDark_Oracle-build\target`、探测 MSYS2 与 Vulkan SDK、探测 CUDA。产物路径在脚本结尾打印（当前是 `D:\DeepDark_Oracle-build\target\release\bundle\nsis\`）。

**前置条件**：`tools\node` 在 PATH 里（已追加进用户 PATH）。

**注意**：所有含中文的 `.ps1` 必须带 UTF-8 BOM，否则 PowerShell 5.1 按 GBK 读会报「意外的标记 }」。检查：`head -c 3 文件名 | od -An -tx1` 应为 `ef bb bf`。

---

## 十一、回滚

卸载 v0.25 后装回 v0.24 即可。**数据目录不用动**：这一版没有改 SQLite schema，也没有迁移向量文件格式；`gpu_report.log` 是新增的独立文件，旧版本不读它。

如果你不希望新版本用显卡（例如怀疑 Vulkan 驱动有问题），启动时加 `--skip-cuda-check` 只跳过 CUDA 校验；要彻底关掉 GPU 推理，在「设置 → 模型配置」里把 GPU 硬件选成「自动选择」并配合小显存档位，或在源码侧设 `DEEPAISEE_VULKAN=0` 重新构建。