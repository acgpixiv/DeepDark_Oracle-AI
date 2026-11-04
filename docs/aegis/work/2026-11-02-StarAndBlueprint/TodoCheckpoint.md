# TodoCheckpoint

状态：**七项改造收尾**（2026-11-03 20:44）。源码全部落盘、release 与安装包已重建、真机 GUI 验证通过；需求③ 的 CUDA 部分按用户指示暂缓并写入交接单。

## 清单
1. [x] 勘察：前端/后端/GPU/数据规模全部读完
2. [x] 写 TaskIntent / BaselineReadSet / TodoCheckpoint / ResumeState
3. [x] 需求① 2D 蓝图：model.ts `BLUEPRINT_CONFIG`、engine.ts 网格底、glRenderer.ts 配色、GalaxyCanvas.vue 白底硬阴影面板
4. [x] 需求② 聚合热点：tree_level / tree_roots / tree_search / graph_data 换按层查询；fold_dir_children 迁到 db.rs 供 star.rs 复用；expand_dir 折叠化
5. [x] 需求③ 的 GPU 部分：ORT DirectML EP 接通（ai/ort.rs 的 detect_ep / attach_ep），CLIP 实测 EP=DML；CUDA 暂缓（见 HANDOFF v0.27 章）
6. [x] 需求④ d3-force node not found：engine3d.ts 的 graphLinks() 规范化 link id + 过滤残边
7. [x] 需求⑤ 盘符恒星：star.rs build_drive_level + engine3d.ts assignOrbits 两两互绕
8. [x] 需求⑥ 文件夹行星：kind 行星环 + folder/file 卫星环；GalaxyView dir-limit=24 / starFileLimit 上限 24
9. [x] 需求⑦ 队列污染：db.rs pending_cond / kind_scope 按 kind 过滤（47,691 张图片不再霸占文本队列）
10. [x] 需求⑦ 吞吐：hash.rs image_vec_from 64×64 降采样；index.rs first_image 放宽 + process_image_batch 选择器 + CLIP 批量前向
11. [x] 需求⑦ CLI：cli.rs 去掉两次白加载模型（saved_dim_of + 惰性 text_embedder）、cmd_index 默认等待 + 进度刷屏 + 打印真实模型行
12. [x] 中文字节切片 panic：db.rs fold_dir_children 改 get()（原来点开中文目录直接 abort 整个进程）
13. [x] release 构建 + 安装包（20:37，exe 57,781,760 B / 安装包 34,678,414 B）
14. [x] GUI 复验：CDP 点开 D → 其他 → 软件 → 触手迷宫1.1 三层中文目录不闪退
15. [x] HANDOFF.md 追加 v0.27 增量章节（含 CUDA 暂缓的接手步骤）

## 当前状态
全部清单项已完成。没有已知的阻塞项。

## 遗留（不建议现在做）
- CUDA 版需要在装有 MSVC x64 + CUDA Toolkit 的机器上构建，见 HANDOFF 的「将来要补 CUDA 版时的接手步骤」。
- `cargo test --lib` 在本机 GNU 工具链下跑不起来（0xc0000139），是环境限制；单元测试代码本身已随 db.rs 的新增用例更新。