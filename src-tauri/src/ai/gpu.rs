//! GPU 能力探测、后端选择与显存分档（需求 REQ-093 ~ REQ-102）。
//!
//! 需求表是按 Python + PyTorch 写的：`torch.cuda.is_available()`、
//! `onnxruntime-directml`、`--skip-cuda-check` 之类。本项目是 Tauri + Rust +
//! llama.cpp，没有 Python 运行时，能用的加速后端在编译期就固定下来了，
//! 所以这里做的是"等价实现"：
//!
//! * 后端来源 = 已链接进 exe 的 GGML 后端注册表（CUDA / Vulkan / Metal / CPU）。
//! * 显存来源 = 后端注册的设备属性（total/free），不是 torch.cuda.mem_get_info。
//! * CUDA Toolkit 校验只对"构建时开启了 CUDA 后端"的版本有意义；GNU 工具链
//!   下 CUDA 根本编不进来，此时报告会明确写清楚"当前安装包没有 CUDA 后端"，
//!   而不是假装检测通过。
//!
//! 所有探测步骤都必须是"失败即降级"，不能返回 Err：GPU 检测失败绝不该让
//! 主程序起不来（REQ-093 的明确要求）。

use std::path::{Path, PathBuf};

/// 显存分档（REQ-097）。
///
/// 阈值与 `native/llama_shim.cpp` 里 `ds_llama_device_tier` 的判定必须逐字一致，
/// 否则界面报告"显存够用"、实际加载却退回 CPU，用户会觉得检测结果是骗人的。
pub const TIER_FULL_VRAM_MB: u64 = 8 * 1024;
pub const TIER_MID_VRAM_MB: u64 = 4 * 1024;
pub const TIER_LOW_VRAM_MB: u64 = 1 * 1024;

/// 把显存容量映射到 0~3 档。
///
/// * 0 = 8 GiB 及以上，全量下放 GPU。
/// * 1 = 4~8 GiB，全量下放但建议量化模型。
/// * 2 = 1~4 GiB，只适合小模型，视觉塔可能放不下。
/// * 3 = 小于 1 GiB，等同于纯 CPU。
pub fn classify_tier(vram_mb: u64) -> u8 {
    if vram_mb == 0 {
        // 显存未知（部分集显/虚拟显卡不报），保守按"够用但不大"处理。
        return 2;
    }
    if vram_mb >= TIER_FULL_VRAM_MB {
        0
    } else if vram_mb >= TIER_MID_VRAM_MB {
        1
    } else if vram_mb >= TIER_LOW_VRAM_MB {
        2
    } else {
        3
    }
}

/// 按显卡型号推断厂商（REQ-098 / REQ-099 / REQ-100 / REQ-101 的分支依据）。
pub fn classify_vendor(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.contains("nvidia") || n.contains("geforce") || n.contains("rtx") || n.contains("quadro") {
        "nvidia"
    } else if n.contains("amd") || n.contains("radeon") || n.contains("ati ") {
        "amd"
    } else if n.contains("intel") || n.contains("arc ") || n.contains("iris") || n.contains("uhd graphics") {
        "intel"
    } else if n.contains("apple") || n.contains("m1") || n.contains("m2") || n.contains("m3") || n.contains("m4") {
        "apple"
    } else {
        "unknown"
    }
}

/// 单个加速设备的自描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub vendor: String,
    pub total_mb: u64,
    pub free_mb: u64,
    pub integrated: bool,
    /// 显存分档 0~3，见 [`classify_tier`]。
    pub tier: u8,
}

/// 编译期链入的后端。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackendInfo {
    pub name: String,
    pub accelerator: bool,
    pub device_count: i32,
    pub features: Vec<(String, String)>,
}

/// GPU 能力报告。字段名刻意贴近需求表 REQ-093 的
/// `{backend, gpu_name, vram_mb, cuda_version, status}`，方便对照验收。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuReport {
    pub generated_at: String,
    pub platform: String,
    pub arch: String,
    /// 实际会被使用的后端名，例如 `vulkan` / `cuda` / `metal` / `cpu`。
    pub backend: String,
    pub gpu_name: String,
    pub vendor: String,
    pub vram_mb: u64,
    pub vram_free_mb: u64,
    /// 系统里能找到的 CUDA Toolkit 版本（nvcc），没有则为 null。
    pub cuda_version: Option<String>,
    /// 这个安装包是否需要 CUDA Toolkit（构建时是否开了 CUDA 后端）。
    pub cuda_required: bool,
    pub driver_version: Option<String>,
    /// `ready` / `degraded` / `cpu_only`。
    pub status: String,
    pub tier: u8,
    /// 一句话说明为什么是这个状态，直接展示给用户。
    pub reason: String,
    pub backends: Vec<BackendInfo>,
    pub devices: Vec<DeviceInfo>,
    /// 额外诊断信息（缺驱动、装了什么可选组件等）。
    pub notes: Vec<String>,
    /// 按显卡型号给出的 CUDA Toolkit 版本建议（REQ-095）。
    pub cuda_hint: Option<String>,
    /// 是否装过可选的 ONNX Runtime 扩展（REQ-098 / REQ-102）。
    pub ort_present: bool,
    /// 装了就会按这个顺序协商 ExecutionProvider。
    pub ort_providers: Vec<String>,
}

impl GpuReport {
    /// 是否真的会把计算放到显卡上。界面拿它决定要不要显示"GPU 加速中"。
    pub fn accelerated(&self) -> bool {
        self.backend != "cpu" && self.tier < 3
    }
}

fn which(program: &str) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let direct = dir.join(program);
            if direct.is_file() {
                return Some(direct);
            }
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{program}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn run_capture(program: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // 0x0800_0000 = CREATE_NO_WINDOW：否则每次探测都会闪一个黑框，
        // 用户会以为程序在后台偷偷执行命令。
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 解析 `nvcc --version` 里的 `release 12.4`（REQ-094）。
fn parse_cuda_version(text: &str) -> Option<String> {
    let marker = "release ";
    let idx = text.find(marker)? + marker.len();
    let rest = &text[idx..];
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if ver.is_empty() {
        None
    } else {
        Some(ver)
    }
}

/// 探测本机 CUDA Toolkit（REQ-094）。找不到时返回 None，不报错。
pub fn detect_cuda_toolkit() -> Option<String> {
    if let Some(nvcc) = which("nvcc") {
        if let Some(text) = run_capture(&nvcc, &["--version"]) {
            if let Some(v) = parse_cuda_version(&text) {
                return Some(v);
            }
        }
    }
    // nvcc 不在 PATH 但设置了 CUDA_PATH 时，直接读版本文件。
    for key in ["CUDA_PATH", "CUDA_HOME"] {
        if let Ok(root) = std::env::var(key) {
            let root = PathBuf::from(root);
            let version_file = root.join("version.txt");
            if let Ok(text) = std::fs::read_to_string(&version_file) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            let nvcc = root.join("bin").join("nvcc.exe");
            if let Some(text) = run_capture(&nvcc, &["--version"]) {
                if let Some(v) = parse_cuda_version(&text) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// 驱动版本：NVIDIA 走 nvidia-smi，其余返回 None（Vulkan 驱动版本由
/// `vulkaninfo` 才能拿到，成本高且格式不稳定，不在这里解析）。
fn detect_driver_version() -> Option<String> {
    let smi = which("nvidia-smi")?;
    let text = run_capture(
        &smi,
        &[
            "--query-gpu=driver_version",
            "--format=csv,noheader",
        ],
    )?;
    text.lines()
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
}

/// NVIDIA 显卡型号与显存（仅在 GGML 后端没报设备时兜底用）。
fn probe_nvidia_smi() -> Option<(String, u64)> {
    let smi = which("nvidia-smi")?;
    let text = run_capture(
        &smi,
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader",
        ],
    )?;
    let line = text.lines().map(|l| l.trim()).find(|l| !l.is_empty())?;
    let (name, mem) = line.split_once(',')?;
    let mb: u64 = mem
        .trim()
        .trim_end_matches("MiB")
        .trim()
        .parse()
        .unwrap_or(0);
    Some((name.trim().to_string(), mb))
}

/// AMD ROCm 是否可用（REQ-099，Linux 为主）。
fn probe_rocm() -> bool {
    if which("rocminfo").is_some() || which("rocm-smi").is_some() {
        return true;
    }
    if std::env::var("ROCM_PATH").is_ok() {
        return true;
    }
    Path::new("/opt/rocm").is_dir() || Path::new("/sys/module/amdgpu").is_dir()
}

/// Intel oneAPI / OpenVINO 是否装了（REQ-100 的 XPU 分支兜底）。
fn probe_intel_toolkit() -> bool {
    for key in ["ONEAPI_ROOT", "INTEL_EXTENSION_FOR_PYTORCH_PATH", "OPENVINO_DIR"] {
        if std::env::var(key).is_ok() {
            return true;
        }
    }
    Path::new("C:\\Program Files (x86)\\Intel\\oneAPI").is_dir()
}

fn now_stamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn collect_backends() -> Vec<BackendInfo> {
    #[cfg(feature = "llama")]
    {
        crate::ai::llama::gpu_backends()
            .iter()
            .map(|b| BackendInfo {
                name: b.name.clone(),
                accelerator: b.accelerator,
                device_count: b.device_count,
                features: b.features.clone(),
            })
            .collect()
    }
    #[cfg(not(feature = "llama"))]
    {
        Vec::new()
    }
}

fn collect_devices() -> Vec<DeviceInfo> {
    #[cfg(feature = "llama")]
    {
        crate::ai::llama::gpu_devices()
            .into_iter()
            .map(|d| {
                let total_mb = d.total_memory / (1024 * 1024);
                let free_mb = d.free_memory / (1024 * 1024);
                DeviceInfo {
                    vendor: classify_vendor(&d.name).to_string(),
                    tier: classify_tier(total_mb),
                    id: d.id,
                    name: d.name,
                    backend: d.backend,
                    total_mb,
                    free_mb,
                    integrated: d.integrated,
                }
            })
            .collect()
    }
    #[cfg(not(feature = "llama"))]
    {
        Vec::new()
    }
}

/// 采集一份完整的 GPU 能力报告。
///
/// 任何一步失败都只是让报告"降级"，不会返回 Err —— 这是 REQ-093 的硬要求：
/// 没有 GPU 的机器必须能正常启动，只是把加速关掉。
pub fn probe() -> GpuReport {
    let mut notes: Vec<String> = Vec::new();

    let backends = collect_backends();
    let mut devices = collect_devices();

    // 独立显卡优先，其次显存大的。集显经常报得比独显"大"（共享内存），
    // 按显存直接排序会把 Intel UHD 排到 RTX 前面。
    devices.sort_by(|a, b| {
        a.integrated
            .cmp(&b.integrated)
            .then(b.total_mb.cmp(&a.total_mb))
    });

    let cuda_version = detect_cuda_toolkit();
    let driver_version = detect_driver_version();

    let cuda_required = backends
        .iter()
        .any(|b| b.name.eq_ignore_ascii_case("cuda") || b.name.eq_ignore_ascii_case("CUDA"));

    let mut backend = "cpu".to_string();
    let mut gpu_name = String::new();
    let mut vendor = String::new();
    let mut vram_mb = 0u64;
    let mut vram_free_mb = 0u64;
    let mut tier = 3u8;
    let mut status = "cpu_only".to_string();
    let mut reason = String::from("未检测到可用的 GPU 加速设备，将使用 CPU 推理。");

    if let Some(best) = devices.first() {
        gpu_name = best.name.clone();
        vendor = best.vendor.clone();
        vram_mb = best.total_mb;
        vram_free_mb = best.free_mb;
        tier = best.tier;
        backend = best.backend.to_lowercase();
        if tier >= 3 {
            status = "cpu_only".into();
            reason = format!(
                "{} 显存仅 {} MiB，低于 1 GiB 阈值，已自动退回 CPU 推理。",
                best.name, best.total_mb
            );
        } else if tier == 2 {
            status = "degraded".into();
            reason = format!(
                "{} 显存 {} MiB，可运行小型嵌入模型，但视觉/音频扩展可能放不下。",
                best.name, best.total_mb
            );
        } else {
            status = "ready".into();
            reason = format!(
                "{} 显存 {} MiB，GPU 加速已启用（{} 后端）。",
                best.name, best.total_mb, best.backend
            );
        }
    } else if let Some((name, mem)) = probe_nvidia_smi() {
        // 后端没报设备但机器上有 NVIDIA 卡：最常见的成因是当前安装包
        // 没编进 CUDA/Vulkan 后端。说清楚这件事比报"没有显卡"有用得多。
        notes.push(format!(
            "系统检测到 NVIDIA 显卡（{}，{} MiB），但当前安装包没有可用的 GPU 后端。",
            name, mem
        ));
        if !cuda_required {
            notes.push(
                "当前构建未启用 CUDA 后端：Windows GNU 工具链无法链接 CUDA，本版 GPU 加速走 Vulkan。"
                    .to_string(),
            );
        }
        gpu_name = name;
        vendor = "nvidia".to_string();
        vram_mb = mem;
    }

    if backends.is_empty() {
        notes.push("未能枚举 GGML 后端注册表，按纯 CPU 处理。".to_string());
    }
    if cuda_required && cuda_version.is_none() {
        // REQ-094 / REQ-095：不给"去装一下"这种废话，直接给型号对应的版本建议。
        notes.push(format!(
            "本安装包启用了 CUDA 后端，但未找到 CUDA Toolkit（nvcc）。建议安装 {}。",
            recommend_cuda(&gpu_name)
        ));
        notes.push(
            "安装后重新运行 DeepDark_Oracle.exe --cli gpu 可确认是否识别到 Toolkit。".to_string(),
        );
    }
    if vendor == "amd" && probe_rocm() {
        notes.push("检测到 ROCm 环境；Windows 下的 AMD 显卡优先走 Vulkan 或 DirectML。".to_string());
    }
    if vendor == "intel" && probe_intel_toolkit() {
        notes.push("检测到 Intel oneAPI/OpenVINO 组件。".to_string());
    }
    if vendor == "apple" {
        notes.push("Apple Silicon 由 Metal 后端加速，无需 CUDA。".to_string());
    }

    // 先算出来：结构体里 gpu_name / cuda_version 是 move 进字段的，
    // 放进初始化列表之后再借用就会报 E0382。
    let cuda_hint = if cuda_required && cuda_version.is_none() {
        Some(recommend_cuda(&gpu_name).to_string())
    } else {
        None
    };

    // REQ-098 / REQ-102：ONNX Runtime 是可选扩展（设置 → 可选扩展里单独装），
    // 没装就是空的。这里只报"装没装 + 会按什么顺序挑 provider"，不假装装了就一定
    // 能跑 GPU —— DirectML 需要 onnxruntime-directml 那个变体，普通 CPU 版装了
    // 也不会出现 DirectMLExecutionProvider，所以顺序只是"协商意向"。
    let ort_present = onnx_runtime_present(&crate::config::data_dir(None));
    let ort_providers: Vec<String> = if ort_present {
        ORT_PROVIDER_ORDER.iter().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };

    GpuReport {
        generated_at: now_stamp(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        backend,
        gpu_name,
        vendor,
        vram_mb,
        vram_free_mb,
        cuda_version,
        cuda_required,
        driver_version,
        status,
        tier,
        reason,
        cuda_hint,
        ort_present,
        ort_providers,
        backends,
        devices,
        notes,
    }
}

/// 把报告写到 `dir/gpu_report.log`（REQ-093 要求的落盘位置）。
///
/// 用 .log 后缀但内容是 JSON：需求表点名要 `gpu_report.log`，而机器可读的
/// JSON 比自由文本更有用（用户贴给开发者时一眼能看清后端和设备）。
pub fn write_report(dir: &Path) -> std::io::Result<PathBuf> {
    write_report_of(dir, &probe())
}

/// 把指定报告落盘（用于 `--skip-cuda-check` 这类改过结论文本的场景）。
pub fn write_report_of(dir: &Path, report: &GpuReport) -> std::io::Result<PathBuf> {
    let path = dir.join("gpu_report.log");
    let body = serde_json::to_string_pretty(report)
        .unwrap_or_else(|_| "{\"status\":\"unknown\"}".to_string());
    std::fs::write(&path, body)?;
    Ok(path)
}

/// 按显卡型号给出 CUDA 版本建议（REQ-095 的等价实现）。
///
/// 原需求是按 RTX 40/30/20 系列推荐 PyTorch 的 cu121/cu118 轮子；本项目不做
/// PyTorch，这里改成"该装哪个 CUDA Toolkit 才能编出带 CUDA 后端的版本"。
pub fn recommend_cuda(gpu_name: &str) -> &'static str {
    let n = gpu_name.to_ascii_lowercase();
    let is_40 = n.contains("rtx 40") || n.contains("rtx 50") || n.contains("l40");
    let is_30 = n.contains("rtx 30") || n.contains("a100") || n.contains("a10");
    if is_40 {
        "CUDA 12.1 及以上（推荐 12.4）"
    } else if is_30 {
        "CUDA 11.8 及以上（推荐 12.1）"
    } else {
        "CUDA 11.8 及以上"
    }
}

/// ONNX Runtime 的 ExecutionProvider 优先级（REQ-098 / REQ-102）。
///
/// 目前 `ai/ort.rs` 只做 CPU 推理，这里先把"应该按什么顺序找 provider"
/// 固化下来，等 ONNX 扩展真的装上时直接按此顺序协商，避免各处顺序不一致。
pub const ORT_PROVIDER_ORDER: [&'static str; 3] = [
    "CUDAExecutionProvider",
    "DirectMLExecutionProvider",
    "CPUExecutionProvider",
];

/// 本机是否装过可选的 ONNX Runtime 扩展（设置 → 可选扩展）。
pub fn onnx_runtime_present(data_dir: &Path) -> bool {
    if which("onnxruntime.dll").is_some() {
        return true;
    }
    data_dir.join("onnxruntime.dll").is_file()
        || data_dir.join("extensions").join("onnxruntime.dll").is_file()
}