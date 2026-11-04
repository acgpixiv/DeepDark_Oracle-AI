# ResumeState

## 上次中断点
2026-11-03 20:44 七项改造收尾。release 与安装包已重建，GUI 真机验证通过。无待办。

## 复现命令
- 前端构建（必须覆写 TEMP，否则 esbuild 报 Access is denied）：
  ```bash
  mkdir -p .tmpbuild && TEMP="$(pwd)/.tmpbuild" TMP="$(pwd)/.tmpbuild" npm run build
  ```
- Rust 检查：`powershell -NoProfile -ExecutionPolicy Bypass -File ./_tmp_check.ps1`，看 CARGO_EXIT=0。
  **注意 `_tmp_check.log` 是 UTF-16LE**，读错误行要 `iconv -f UTF-16LE -t UTF-8 _tmp_check.log | grep -n 'error'`。
- 打包：`powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-release.ps1`，产物在 `D:/DeepDark_Oracle-build/target/release/bundle/nsis/`。**构建前必须先 `taskkill //F //IM DeepDark_Oracle.exe`**，否则 cargo 报 `failed to remove file ... 拒绝访问 (os error 5)`。
- GUI 真机验证：`cd /d/DeepDark_Oracle-build/target/release && nohup env WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9333" ./DeepDark_Oracle.exe > /tmp/gui.log 2>&1 & disown`，sleep 10 后 `curl -s http://127.0.0.1:9333/json/list`；脚本见 `.tmpbuild/_cdp_cn2.mjs`。

## 本轮产物
- exe：`D:/DeepDark_Oracle-build/target/release/DeepDark_Oracle.exe`，57,781,760 B，SHA-256 `23d07b849854292c41698aace384e27fbac641b5bb7b62ec368da6e8d07cd87f`。
- 安装包：`bundle/nsis/DeepDark_Oracle_0.0.26_x64-setup.exe`，34,678,414 B（33.07 MiB），SHA-256 `7e9ee4227a46a028675fb832fb486c270621d03a8b203f4edbf4a083439c0a00`。
- 根目录 `WebView2Loader.dll` 已补回（160,880 B）。

## 关键结论（勿重复踩）
- 模型**确实**上了 GPU（+1.7~1.9 GiB 显存台阶）。之前「没上 GPU」的结论是采样方法错误导致的。
- 采样必须用 `nvidia-smi --loop-ms=150/200`（单次调用要 0.48 s）；输出有 `\r`，解析前 `tr -d '\r'`；本机没有 `bc`。
- 图片索引的真瓶颈是 `hash.rs image_vec_from` 的全尺寸 RGB 遍历，不是 Db 锁也不是 sync_channel。
- `db.rs` 里对含中文的路径做前缀判断**必须用 get() 而不是字节切片**，否则 panic 会 abort 整个 Tauri 进程。
- CUDA 在 GNU 工具链下被 `build.rs:26-28` 硬阻断，必须有 MSVC；本机无 VS、无 CUDA Toolkit、C 盘只剩 20 GB。
- `src-tauri/build.rs` 的 `cuda_enabled && windows_gnu` panic 是**唯一**的 CUDA 墙，`Cargo.toml` 的 `candle-cuda` feature 本身是可用的。
- `.tools/onnxruntime/onnxruntime.dll` 是 1.20.1 的 **DirectML** 版，导出 `OrtSessionOptionsAppendExecutionProviderEx_DML`；CLIP 能加载靠的就是它。