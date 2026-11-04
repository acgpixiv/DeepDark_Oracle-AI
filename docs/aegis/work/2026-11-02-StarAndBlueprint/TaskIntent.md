# TaskIntent

## 用户请求（原始，m00002）
对 D:/软件/DeepDark_Oracle 做 7 项改造：
1. 2D 星图改成清晰平面/蓝图风格（依据「清晰平面与蓝图风格需求明细表.xlsx」15 条）
2. 导入数据 > 1TB 后整个软件变卡，要优化
3. 接入 candle CUDA 后端，支持 safetensors 模型
4. 3D 星图里的星星点不开，一点开就报错
5. 3D 星图里把 C/D/E 盘盘符改成恒星，要有引力；一个盘自转，三个盘三体运动，四个盘四体
6. 点开节点会冒出一大堆行星；文件夹就是行星，也要有引力、自转、公转
7. 加强 GPU 调用能力，当前索引文件时 GPU 占用率几乎为零

## 非目标
- 不改动搜索/向量语义逻辑的正确性
- 不引入新的重型前端依赖（不乱加 three.js 之外的库）

## 硬约束（本机事实）
- 无 Visual Studio / 无 CUDA Toolkit / 无 nvcc；GNU 工具链（MSYS2 UCRT64）
- build.rs 在 cuda_enabled && windows_gnu 时 panic
- 项目路径含非 ASCII「软件」，必须用 activate-local.ps1 镜像工具链
- WebView2Loader.dll 必须留在项目根
- 构建：. .\scripts\activate-local.ps1; npm run tauri:release；产物在 D:/DeepDark_Oracle-build/target/release/bundle/nsis/
- approval policy = never，不能提权

## 验收证据
- cargo check / tsc 通过
- npm run build 通过
- 出安装包并可启动（截图自检）

## 停止状态
done | blocked | needs-verification
