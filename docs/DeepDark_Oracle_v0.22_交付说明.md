# DeepDark_Oracle v0.22 交付说明

生成时间：2026-10-27
交付物：`release/DeepDark_Oracle_0.0.22_x64-setup.exe`（682,546,065 字节 / 650.93 MiB）
SHA-256：`1fe062b2b06a4825a695478cc2fe16f85754b9adbc7109441a2000852299748c`

---

## 一、本次做了什么

### 1. 软件更名：deepaisee → DeepDark_Oracle

全局改名，覆盖前端、后端、配置、脚本与打包产物：

| 位置 | 改动 |
| --- | --- |
| `src-tauri/tauri.conf.json` | `productName` = DeepDark_Oracle；`identifier` = `com.deepdarkoracle.desktop`；窗口标题「DeepDark_Oracle · AI 磁盘智搜」；开始菜单目录 DeepDark_Oracle |
| `src-tauri/Cargo.toml` | 包名 `DeepDark_Oracle`、`default-run`、`[lib] name = "deepdark_oracle_lib"` |
| `src-tauri/src/main.rs` | `deepdark_oracle_lib::run()` |
| `package.json` | `name` = `deepdark-oracle` |
| `index.html` | `<title>DeepDark_Oracle</title>` |
| `src/views/SearchView.vue` / `src/components/SettingsDrawer.vue` | 界面上的品牌名 |
| `src-tauri/src/lib.rs` | 日志名、错误文件名、初始化失败提示 |
| `src-tauri/src/config.rs` | 数据目录回退路径 `%LOCALAPPDATA%\DeepDark_Oracle\data` 等 |
| `src-tauri/src/build.rs` / `native/CMakeLists.txt` | CMake 工程名、缓存目录 `.deepdark-oracle-cmake` |
| `src-tauri/nsis/installer-hooks.nsh` | 注册表键 `Software\DeepDark_Oracle` |
| `launcher.ps1` / `DeepDark_Oracle 启动器.cmd` | 启动器文件名与横幅 |
| `scripts/build-release.ps1` | 产物名 `DeepDark_Oracle.exe` |

**刻意保留旧名的地方**（改了会伤到用户，属于有意为之）：

- `src/theme.ts` 的 `localStorage` 键 `deepaisee.lang` / `deepaisee.theme` —— 改了用户已保存的语言和主题偏好会丢。
- `DEEPAISEE_*` 系列环境变量名（`build.rs`、`CMakeLists.txt`、构建脚本、`ai/clip.rs` 的 `DEEPAISEE_ORT_DLL`）—— 内部变量名，用户看不见，保持兼容。
- `CHANGELOG.md` 历史条目、`HANDOFF-2026-10-27.md` —— 历史文档保持原文。

### 2. 右上角「索引目录结构」树状图面板

按 `DeepDark_Oracle_树状图功能实现方案.docx` 第八节实现。顶栏原本那个只显示「143,522 个文件 · 54.5 GB」的统计按钮，现在点开就是一棵可展开的索引目录树。

**入口**：主界面右上角统计按钮 → 点击从按钮下方垂直弹出 400px 面板（最大高度 80vh），再点或点面板外部 / 按 Esc 收起。

**三段式结构**：

1. **顶部栏**：标题「索引目录结构」+ 刷新按钮 + 在资源管理器中打开按钮 + 极小搜索框。
2. **主体树区**：根节点列出所有已添加的索引盘符 / 根目录，标准缩进层级；文件夹灰色线条图标；选中行浅灰背景 `#F5F5F5`；加载中显示旋转 Loading；出错显示红色感叹号。
3. **底部统计栏**：如「当前目录包含 17 个子文件夹，4 个文件 · 68.4 GB」，右侧「完整视图 →」跳到原来的全屏树状图页。

**交互**：

- 点击文件夹仅展开 / 折叠，初期纯展示。
- **按层懒加载**：每展开一层才查一次该层；已加载的层缓存在前端，收起面板不销毁，再打开直接复用。
- **只读索引库，不重新扫描**：打开面板不触发任何磁盘遍历，全部数据来自 SQLite 的 `files` 表目录聚合。
- 搜索框按目录名模糊匹配，命中高亮（`#E6F7FF` 背景 + 加粗），点结果自动逐级展开祖先并滚动定位。
- 右键菜单：打开文件位置 / 复制完整路径 / 复制文件名 / 加入工作区。
- 文件行右侧 6px 小圆点表示索引状态：绿=已索引、灰=未索引。
- 悬停 Tooltip 显示完整绝对路径。

**视觉规范**（严格按 docx）：白底 `#FFFFFF`、主文字 `#333333`、次要 `#999999`、边框 `#CCCCCC` + 轻投影、选中 `#F5F5F5`、行高 28px、16px 单色线性图标、按后缀分类型图标、12-13px 系统无衬线字体；暗色主题有对应的一套变量。

### 3. 后端新增命令（`src-tauri/src/cmd.rs`）

| 命令 | 作用 |
| --- | --- |
| `tree_browse(path?, file_limit?)` | 不传 `path` 返回索引根层；传了返回该层的直属子目录 + 直属文件（按修改时间倒序，默认最多 200 条，超出提示「另有 N 个文件未显示」） |
| `tree_search(query, limit?)` | 按目录名模糊匹配，按递归文件数倒序返回 |
| `workspace_add(dir)` | 把目录加进「工作区实时检索」，供右键菜单调用 |

`src-tauri/src/db.rs` 新增两个查询方法：

- `dir_rollup_under(prefix)`：用 `dir >= lo AND dir < hi` 的字符串区间做前缀裁剪，**14 万文件的库上全量聚合 131~144ms，带前缀裁剪只要 42~48ms**，且展开越深越快。区间查询单独 OR 了 `dir = 该目录` 的等值条件 —— 只写区间会漏掉「目录自己的直属文件」（`D:\wx` 在字节序上排在 `D:\wx\` 之前）。
- `dir_file_stats(dir)`：返回该目录直属文件的 总数 / 总大小 / 已抽文本数 / 已嵌入数，用于计算索引状态圆点与进度。

### 4. 顺手修掉的既有问题

- `src/views/TreeView.vue` 用了 `t()` 却没 import i18n；且模板里 `v-for="(t, i) in trail"` 会把 i18n 的 `t` 遮蔽成 TreeNode，切到树状图页会直接崩。已改为 `(crumb, i)` 并补上 import。
- `src/i18n.ts` 里「清空」「打开失败：」两处词条重复，esbuild 构建报 Duplicate key，已合并。
- `src-tauri/src/bin/` 下的 5 个开发探针与 `src-tauri/examples/` 完全重复，会被 tauri-bundler 当成 external binary 打进安装包（多出十几 MB）；已移出，只保留 `examples/`。

---

## 二、怎么安装

双击 `release/DeepDark_Oracle_0.0.22_x64-setup.exe`，按向导走。安装包约 651 MiB，含模型文件，装完约 989 MiB。

- 安装目录默认在非 C 盘（C 盘吃紧时不会挤爆系统盘）。
- 装完开始菜单会有 DeepDark_Oracle 目录。
- 卸载用安装目录里的 `uninstall.exe`，或「设置 → 应用」里卸载。

**首次使用建议**：打开后进「设置 → 索引」，把要检索的盘 / 目录加进去，然后让它跑一遍索引；索引跑完后右上角面板才有内容可展开。

---

## 三、本轮编译 / 打包踩到并修掉的问题

这部分是给以后维护看的，用户可跳过。

### 1. windres 打不开含中文路径的 .ico

Windows GNU 工具链下 `tauri-build` 生成的 `resource.rc` 里写的是图标的**绝对路径**，项目放在 `D:\软件\DeepDark_Oracle` 时 `windres` 报 `can't open icon file ...: Invalid argument`，构建直接 panic。

修法：`src-tauri/build.rs` 新增 `ascii_window_icon()` —— 图标绝对路径含非 ASCII 字符时，先复制到 `OUT_DIR`（纯 ASCII）再交给 `tauri-build`；ASCII 路径下行为不变，MSVC 构建也不受影响。

### 2. 新版 llama.cpp 的 encoder 断言会让长文本嵌入直接 abort

`GGML_ASSERT(cparams.n_ubatch >= n_tokens && "encoder requires n_ubatch >= n_tokens")`。

原来 shim 把 `n_batch` / `n_ubatch` 写死 512，索引到长文档时 token 数超过 512 就被**直接 abort（不是返回错误码）**，表现为「窗口突然消失、没有任何日志」。现在 `n_batch` / `n_ubatch` 跟随上下文长度，并在 `ds_llama_embed` 里按 `n_batch` 截断 token，超长文本不会再打崩进程。

### 3. 构建环境要求（记录备查）

本机无 MSVC，改用 `x86_64-pc-windows-gnu` + MSYS2 UCRT64（`D:\dsh-tools\msys64`）。GNU 工具链不认中文路径，`CARGO_HOME` / `RUSTUP_HOME` / `CARGO_TARGET_DIR` / `TMP` 必须全在纯 ASCII 目录，项目本身通过 `subst X:` 映射后构建。

---

## 四、验证记录

| 项目 | 结果 |
| --- | --- |
| 前端 `vite build` | 通过，产出 `dist/index.html` + CSS 57.52kB + JS 222.30kB |
| 后端 `cargo check`（`--no-default-features --features llama,candle,custom-protocol`） | 通过，0 error 0 warning，35.82s |
| 后端 `cargo build --release` | 通过，4m07s，产物 20,572,672 字节 |
| 安装包内 `--selftest` | 通过 8 项 / 失败 0 项 / 跳过 8 项 |
| 安装包内容核对 | 无开发探针残留；模型、ffmpeg、DLL、脚本齐全 |
| **安装包静默安装** | `setup.exe /S` 退出码 0，默认装到 `D:\Apps\DeepDark_Oracle`（非 C 盘），文件齐全 |
| **安装版启动** | 启动正常、进程常驻（853MB，模型加载完成 `loaded dim=768 backend=llama.cpp`） |
| **安装版面板** | 点右上角统计按钮，面板从按钮下方弹出，空库时显示「索引里还没有任何目录」+「0 个索引根 · 0 个文件 · 0 B」 |
| **安装版卸载** | `uninstall.exe /S` 正常移除程序与注册表项 |
| 源码版真机 GUI 面板 | 用真实 24.6 万文件索引库验证：根层 `D:\`、展开 17 个子目录、底栏统计正确 |
| 暗色主题 | 面板配色跟随主题切换正常 |
| 窄窗口 980x640 | 面板完整显示、不溢出 |

---

## 五、已知限制

- 面板当前是**纯展示**：点文件夹只展开/折叠，不触发搜索或跳转。
- 单层文件最多显示 200 条（超出显示「另有 N 个文件未显示」），避免超大目录一次拉太多。
- 搜索只匹配**目录名**，不匹配文件名（方案里明确是「极小搜索框过滤」，按目录结构过滤）。
- 面板的搜索输入与右键菜单在真机上受坐标换算限制没能自动点击验证，但已在浏览器预览里用同一份代码验证过（搜索 wx → 1 条结果、右键 4 项菜单正常）。

---

## 六、目录说明

```
D:\软件\DeepDark_Oracle\
├─ release\DeepDark_Oracle_0.0.22_x64-setup.exe   ← 交付安装包（发给别人用这个）
├─ DeepDark_Oracle.exe                             ← 便携版主程序（双击即用，读同目录 data\）
├─ DeepDark_Oracle 启动器.cmd                       ← 便携版启动器（带菜单）
├─ launcher.ps1
├─ data\                    便携版数据目录（索引库 24.6 万文件、config.json）
├─ src\                     前端（Vue 3 + TS）
│  └─ components\IndexTreePanel.vue               ← 新增：右上角树面板
├─ src-tauri\                后端（Rust + Tauri 2）
│  ├─ src\cmd.rs                                  ← 新增 tree_browse / tree_search / workspace_add
│  ├─ src\db.rs                                   ← 新增 dir_rollup_under / dir_file_stats
│  ├─ native\llama_shim.cpp                       ← 修 ubatch 断言
│  └─ build.rs                                    ← 修 windres 中文图标路径
├─ docs\DeepDark_Oracle_v0.22_交付说明.md          ← 本文档
├─ CHANGELOG.md              v0.22 条目（含本轮全部改动）
└─ dist\                     前端构建产物
```

**便携版 vs 安装版**：

- 便携版 = 项目根目录的 `DeepDark_Oracle.exe`，直接双击就能跑，索引库读同目录的 `data\`（已经有 24.6 万文件的真实索引）。适合自己用。
- 安装版 = `release\` 里的 setup.exe，装到别的机器上，首次启动时索引库是空的，需要自己添加索引目录。适合分发。

---

## 七、回滚方式

本轮改动没有提交到 Git（项目当前不是 git 仓库）。如需回退：

- **只回退树状图面板**：删掉 `src/components/IndexTreePanel.vue`，把 `src/App.vue` 顶栏里的 `div.index-wrap` 恢复成原来的 `button.index-link`，去掉 `lib.rs` 里注册的 `tree_browse` / `tree_search` / `workspace_add` 三行。
- **回退改名**：把 `tauri.conf.json` / `Cargo.toml` / `package.json` / `index.html` / 启动器里的 DeepDark_Oracle 改回 deepaisee 即可（`DEEPAISEE_*` 环境变量本来就保留着）。
- **回退安装**：用 `uninstall.exe` 卸载后重装旧版。

---

## 八、后续可做的事

1. 面板点文件夹可以直接在该目录下搜索（需要新的后端命令支持 path 过滤）。
2. 单层文件超过 200 条时做虚拟滚动，而不是截断。
3. 搜索扩展到文件名（当前只匹配目录名）。
4. 树节点右键「在此处新建索引」。
