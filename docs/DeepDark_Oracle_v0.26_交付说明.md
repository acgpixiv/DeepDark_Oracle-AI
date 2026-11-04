# DeepDark_Oracle v0.26 交付说明

生成时间：2026-11-01
交付物：`release/DeepDark_Oracle_0.0.26_x64-setup.exe`（34,642,746 字节 / 33.04 MiB）
SHA-256：`a8e945321e1f220c587436285921cd97106c7b0adc114e4c6319eac0815530ea`
免安装可执行文件：`DeepDark_Oracle.exe`（57,692,160 字节，SHA-256 `edfc792b2e506b34f4bc27c1bee09c5449edf00e05c1703129555a7828226074`）

---

## 一、这一版解决什么

需求来源是 `2D转3D与星图需求明细.xlsx` 的单表 **33 条需求**（序号 1~33，无 REQ 编号）：

| 分组 | 条目 | 本版做法 |
| --- | --- | --- |
| 2D 转 3D（技术选型 / 数据适配 / 核心渲染 / 性能 / 进阶效果） | 1~7 | `3d-force-graph` + `three` 接管力导向与相机，节点绘制改为 InstancedBufferGeometry |
| 星图场景（深空 / 发光节点 / 粒子 / 连线 / Bloom / 材质 / 贴图 / 动画 / 交互） | 8~16 | 新增 `src/galaxy/star/` 五个文件的完整 3D 星图引擎 |
| 视觉风格（风格定义 / 提示词） | 17~18 | 风格参数落地到代码；提示词另出 `docs/星图视觉参考提示词.md` |
| 渐进式探索（架构 / 数据结构 / 交互 / 视觉 / 导航） | 19~23 | 三级钻取 + 面包屑 + 前进后退 + 图例过滤 |
| 铅绘滤镜（G-Buffer / 边缘 / 质感 / 纸纹 / 整合） | 24~28 | `src/galaxy/star/sketch.ts` 独立 ShaderPass |
| 性能优化（DPR / 实例化 LOD / 纹理 / Pass 开关 / 移动端降级） | 29~33 | 见第七节，其中三条是等价实现 |

配套的交付动作：**把安装包里用不上的陈旧二进制全清掉**，重出一版只有 33 MB 的安装包（详见第六节）。

---

## 二、3D 星图引擎（`src/galaxy/star/`）

| 文件 | 行数 | 职责 |
| --- | --- | --- |
| `engine3d.ts` | 1043 | 场景装配、节点/连线/星尘三套 Instanced 几何、相机、拾取、后期处理、LOD |
| `StarCanvas.vue` | 901 | Vue 外壳：数据加载、钻取历史、HUD、图例、右键菜单、降级 |
| `nebula.ts` | 297 | 星尘粒子云、近邻连线、球壳背景、雾效 |
| `sketch.ts` | 173 | 铅绘滤镜 ShaderPass（Sobel + fbm 等值线 + 纸纹） |
| `shaders.ts` | 174 | glow / dot / cloud 三张程序化贴图 + 节点与粒子着色器 |

合计 2588 行，相关外壳还有 `GalaxyView.vue`（291 行）与后端 `src-tauri/src/star.rs`（582 行）。

### 为什么节点不用一堆 Mesh

`engine3d.ts` 让 `ForceGraph3D` **只负责力学与相机**（`nodeVisibility` 全关），节点绘制交给一块 `InstancedBufferGeometry`：位置、颜色、尺寸、透明度、LOD 全部走实例属性。全场景节点 **1 次 draw call**、连线 1 次、星尘 1 次。这是 REQ #4「千节点不卡」与 REQ #30「实例化渲染」的共同底座。

### 关键参数（与需求表一致）

| 项 | 值 | 出处 |
| --- | --- | --- |
| 背景色 | `#050510` | REQ #17 深空蓝黑 |
| 雾 | `FogExp2`，密度 0.002 | REQ #8 / #17 |
| 节点材质 | `ShaderMaterial` + 加色混合 + 程序化 glow 贴图 | REQ #9 / #13 / #14 |
| 星尘 | 3000 粒，半径 900，球内均匀分布 | REQ #10 |
| 星尘连线 | 空间哈希就近配对，线宽 `1`、alpha 0.06 | REQ #11 |
| Bloom | `UnrealBloomPass`，**strength 0.85 / radius 0.35 / threshold 0.62** | REQ #12（有意偏离，见下） |
| 动画 | `requestAnimationFrame` + 星尘自转 0.08 | REQ #15 |

### 两处必须如实交代的偏离

**(1) Bloom 参数没有照抄需求表的 1.8 / 0.5 / 0.1。** 照抄的实测结果是整屏过曝：亮像素占比 51.2%，画面基本是白的。原因是需求表的参数前提是"背景纯黑 + 少量发光体"，而本项目的深空底色 `#050510` 亮度已经贴着 `threshold 0.1`，加上星云球壳整片参与泛光，叠完就饱和。改成 `threshold 0.62` 后**只有真正的发光体泛光**，实测亮像素降到 4.65%，观感上的辉光仍在。取舍写在 `engine3d.ts` 的 `applyBloom()` 注释里。

**(2) REQ #31 的 KTX2 压缩纹理与纹理图集没有做。** 本项目所有贴图（glow / dot / cloud / 纸纹）都是**运行时 canvas 程序化生成**，磁盘上不存在任何图片资源，没有"多张小图打包成大图"的对象，也没有 KTX2 能省的加载时间。实际做的是：2 的幂尺寸 + `generateMipmaps` + `setAnisotropy(maxAniso)`（`engine3d.ts:254-259`、`nebula.ts:249-256`）。真要上 KTX2 反而要引入 `KTX2Loader` 和一套离线转码工具链，换不出收益。

**(3) REQ #32 的 Mesh BVH 换成了 CPU 屏幕投影拾取。** 一万个球做射线检测本身就是性能陷阱，`engine3d.ts` 注释里写明了「屏幕坐标拾取比 Raycaster 快两个数量级」。

**(4) REQ #13 的 SpriteMaterial 换成了 Instanced 广告牌。** 目的相同（永远朝向相机），但 1 次 draw call 顶掉 N 个 Sprite。

---

## 三、启动时的白屏 BUG 修复（本版实机发现）

出包后第一次实机启动星图，整页显示「界面加载出错」，控制台报：

```
Uncaught Error: An error occurred. See https://github.com/styled-components/polished/...
```

定位链：`3d-force-graph` 依赖的 `three-render-objects` 用 `polished` 解析背景色，而 `polished/lib/color/parseToRgb.js` 第一行就是 `if (typeof color !== 'string') throw`。本项目原来传的是数字 `0x050510`，直接抛错把整棵组件树打挂。

**修法**：`engine3d.ts:73-79` 拆成两个常量 —— `BG_PLAIN = 0x080b14`（给 `scene.background` 用，数字没问题）与 `BG_DEEP_CSS = "#050510"`（给 `backgroundColor()` 用，必须是 CSS 串）。这两条路径混用是这类 BUG 的常见来源。

---

## 四、渐进式探索（REQ #19~23）

| 层级 | 展示 | 数据来源 |
| --- | --- | --- |
| Level 0 | 9 个类型节点（文件夹/文档/图片/代码/视频/音频/压缩包/程序/文本） | `star_overview` 聚合 |
| Level 1 | 某个类型下的目录与文件 | `star_children(nodeId)` |
| Level 2/3 | 继续下钻，每层带 `dirLimit` / `fileLimit` | 同上，逐层拉 |

- **数据与视图分离**（REQ #19）：后端只出 `StarGraph{nodes, links, total}`，前端不缓存跨层数据，靠 300ms 内的 `star_layout_get/put` 复用坐标，避免"钻取回来位置全变"。
- **树状结构**（REQ #20）：`StarNode` 带 `type` / `kind` / `count` / `size` / `depth`，父节点由 `links` 的 `contains` 边表达。
- **钻取交互**（REQ #21）：单击选中、双击钻取（`focusOn` 抬相机 200~300ms 缓动）、右键菜单四项（打开文件 / 展开这一层 / 聚焦 / 复制路径）。
- **多标签连线防混乱**（REQ #22）：连线默认 alpha 0.06，悬停时该节点的边升到 0.45；图例条点一下按类型过滤节点（`toggleLegend`）。
- **面包屑与返回**（REQ #23）：左下角面包屑逐级点击跳转，工具栏 `←`/`→` 走 `history` 栈，`historyAt` 越界时按钮置灰。

---

## 五、铅绘滤镜（REQ #24~28）

`sketch.ts` 导出一个完整的 `ShaderPass`，作为 composer 的最后一个 pass 挂在 Bloom 之后：

| 需求 | 实现 |
| --- | --- |
| #24 G-Buffer | `ensureDepthTexture()` 给 composer 挂一张深度渲染目标，供 Sobel 采样；分辨率按 DPR 取 |
| #25 边缘检测 | 片元着色器里 Sobel 采样深度，梯度超过阈值画线，线条越近越粗 |
| #26 手绘质感 | fbm 噪声偏移 UV（手抖），再用噪声做等值线形成"拓扑线" |
| #27 纸张纹理 | 程序化纸纹按 Multiply 叠加，线条做断续与墨色不匀 |
| #28 整合 | 单一 ShaderPass + uniform 接口，默认**关闭**（设置 → 图形与 GPU 里开） |

默认关闭是有意的：它是"看着玩"的效果，不能影响正常检索的观感。

---

## 六、安装包清理（这次被点名删的东西）

### 删掉的

| 文件 / 目录 | 体积 | 为什么删 |
| --- | --- | --- |
| `.gitattributes` | 163 B | 项目不是 git 仓库，无意义 |
| `libgcc_s_seh-1.dll` | 150,537 B | MSYS2 编译期运行时，exe 已静态链接，运行时不需要 |
| `libwinpthread-1.dll` | 68,731 B | 同上 |
| `libstdc++-6.dll` | 2,659,194 B | 同上 |
| `onnxruntime.dll` | 13,557,800 B | 可选扩展，**不该随包分发**（用户需要时用 `resources/scripts/install-onnxruntime.ps1` 自己装） |
| `vulkan-1.dll` | 724,189 B | 系统自带（`C:\Windows\System32` 有 1,452,392 B 的同名库），实测删掉后程序正常启动 |
| `resources/` | — | 旧的脚本副本，打包取的是 `scripts/*.ps1`，副本里的 `install-onnxruntime.ps1` 还是缺 `param([switch]$Update)` 的旧版 |
| `plugins/` | — | 源码零引用；安装时由 `installer-hooks.nsh` 自动创建 |
| 92 个 `_` 前缀临时脚本 / 截图 / 测试目录 | **79.1 MB** | 开发期取证残留 |

### **保留**的

| 文件 | 为什么不能删 |
| --- | --- |
| `WebView2Loader.dll`（160,880 B） | **实测删掉即启动失败**：`0xC0000135 STATUS_DLL_NOT_FOUND`（退出码 3221225781），窗口一个都不出。它由 cargo 在 `target/release/` 下生成但不会自动复制到项目根 |

### 清理前后

- 根目录从 124 项降到 **31 项**，`_` 前缀残留 **0 个**。
- 安装包 34,642,746 字节；**不含**任何模型权重、索引数据、MinGW 运行时 DLL、vulkan-1.dll、onnxruntime.dll。

---

## 七、性能优化四条（REQ #29~33）

| 条 | 需求原方案 | 本版做法 |
| --- | --- | --- |
| #29 DPR 限制 | `setPixelRatio(min(devicePixelRatio, 2))` | `currentDpr()` 按画质档位夹到 `min(device, 档位上限)`；后处理渲染目标按 DPR 开 |
| #30 实例化 + LOD | `InstancedMesh` + 相机距离切模型 | `InstancedBufferGeometry` 1 次 draw call；`aLod` 实例属性按相机距离每 200ms 重写，远处压暗压小 |
| #31 图集 + KTX2 | Texture Atlas / KTX2Loader | **未做**，理由见第二节（全程序化贴图，无资产可压） |
| #32 Pass 开关 + BVH | `pass.enabled` + Mesh BVH | 拖拽超过 4px 立刻 `setPostSuspended(true)` 关 Bloom 与铅绘，停手 500ms 恢复；拾取改 CPU 屏幕投影 |
| #33 移动端降级 | 设备能力检测动态降参 | `onSlowFrame`：连续掉帧自动关 Bloom → 再关深空，弹一条「帧率偏低，已自动关闭部分特效」并提供恢复按钮 |

**#32 的 500ms 是有讲究的**：需求写"交互停止后恢复"，如果一松手就恢复，用户会在拖拽的顿挫间隙看到 Bloom 反复闪烁。500ms 是实测既不拖沓也不闪的折中。

---

## 八、验证记录

| 项 | 结果 |
| --- | --- |
| `npm run build` | exit 0，2211 modules，11.06s |
| `cargo build --release` | exit 0，4m 01s（GNU 工具链，Vulkan 后端已编入） |
| 免安装 exe 启动 | 窗口 `Tauri Window`「DeepDark_Oracle · AI 磁盘智搜」正常 |
| `Ctrl+G` 进 3D 星图 | 正常渲染，HUD「全部索引 / 9 节点 / 240 fps / 1 批次」 |
| 过曝复检 | 亮像素占比 51.2% → **4.65%** |
| 静默安装 `/S /D=D:\ddo_v26test` | exit 0，7 文件 + 3 目录，139 MB（含 ffmpeg 87 MB） |
| 安装版启动 | 正常出窗口 |
| 静默卸载 `uninstall.exe /S` | exit 0，仅留 `data/` `webview-data/` `plugins/README.txt`（设计如此） |
| 注册表 `HKCU\...\Uninstall\DeepDark_Oracle` | 已清空 |

---

## 九、需求表里**没有**照做的部分

| 条 | 原方案 | 理由 |
| --- | --- | --- |
| #4 移动端 `controlType=orbit` | 移动端手势适配 | 桌面程序，无触摸目标；`OrbitControls` 仍启用，只是不做移动端特化 |
| #13 SpriteMaterial | 每节点一个 Sprite | 换成 Instanced 广告牌，1 draw call 顶掉 N 个 Sprite |
| #31 KTX2 + Atlas | 压缩纹理 | 见第二节，全程序化贴图无资产可压 |
| #32 Mesh BVH | 空间加速结构做射线检测 | 改 CPU 屏幕投影，量级更快 |
| Bloom 参数 | 1.8 / 0.5 / 0.1 | 照抄即过曝，改 0.85 / 0.35 / 0.62，见第二节 |

除此之外 **28 条全部按需求落地**。

---

## 十、怎么用

1. 双击 `release/DeepDark_Oracle_0.0.26_x64-setup.exe`，按向导装（默认当前用户，不需要管理员）。
2. 首次启动按提示导入模型（或直接跳过，纯文件名搜索也能用）。
3. 点顶栏 `Network` 图标或按 `Ctrl+G` 进星图；左上角 `2D / 3D` 切换，右上角工具栏可缩放、看全图、聚焦、前进后退。
4. 设置 → 图形与 GPU → 可切画质档位、关 Bloom、开铅绘滤镜、关深空。

---

## 十一、构建方式

```powershell
npm run tauri:release
# 或
powershell -ExecutionPolicy Bypass -File scripts/build-release.ps1
```

产物在 `D:\DeepDark_Oracle-build\target\release\bundle\nsis\DeepDark_Oracle_0.0.26_x64-setup.exe`（注意是 `bundle/nsis/` 子目录，不在 `target/release/` 根）。构建目标目录因仓库路径含中文被重定向到 `D:\DeepDark_Oracle-build`。

---

## 十二、回滚

上一版是 v0.25：`release/DeepDark_Oracle_0.0.25_x64-setup.exe`，34,276,337 字节，SHA-256 `677aca3e6367cb247e3ea003750081a686c62bbf47a2a06dee7961501912d606`。若需回滚，重新出该版本的包并覆盖安装即可；数据目录（`data/`）格式未变，回滚不会丢索引。
