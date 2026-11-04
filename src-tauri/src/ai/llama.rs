//! llama.cpp 绑定（通过 native/llama_shim.cpp 暴露的极简 C ABI）。
//!
//! 只在 `llama` feature 打开时编译。

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use rayon::prelude::*;

use super::{l2_normalize, EmbedInfo, Embedder};

const MAX_NATIVE_BATCH_SEQUENCES: usize = 32;

#[allow(non_camel_case_types)]
type c_void = std::ffi::c_void;

#[repr(C)]
struct NativeMediaItem {
    kind: c_int,
    data: *const c_void,
    length: usize,
    width: u32,
    height: u32,
    token_budget: c_int,
}

extern "C" {
    fn ds_llama_create(
        model_path: *const c_char,
        n_ctx: c_int,
        n_threads: c_int,
        device_name: *const c_char,
        err: *mut c_char,
        err_len: c_int,
    ) -> *mut c_void;
    fn ds_llama_duplicate_context(
        source: *mut c_void,
        n_ctx: c_int,
        n_threads: c_int,
        with_multimodal: c_int,
        err: *mut c_char,
        err_len: c_int,
    ) -> *mut c_void;
    fn ds_llama_device_count() -> c_int;
    fn ds_llama_device_info(
        index: c_int,
        name: *mut c_char,
        name_len: c_int,
        description: *mut c_char,
        description_len: c_int,
        device_id: *mut c_char,
        device_id_len: c_int,
        physical_id: *mut c_char,
        physical_id_len: c_int,
        backend: *mut c_char,
        backend_len: c_int,
        free_bytes: *mut u64,
        total_bytes: *mut u64,
        device_type: *mut c_int,
    ) -> c_int;
    fn ds_llama_dim(handle: *mut c_void) -> c_int;
    fn ds_llama_capabilities(handle: *mut c_void) -> c_int;
    fn ds_llama_detail(handle: *mut c_void, out: *mut c_char, out_len: c_int);
    fn ds_llama_audio_sample_rate(handle: *mut c_void) -> c_int;
    fn ds_llama_embed_media(
        handle: *mut c_void,
        items: *const NativeMediaItem,
        count: c_int,
        out: *mut f32,
        out_len: c_int,
        err: *mut c_char,
        err_len: c_int,
    ) -> c_int;
    fn ds_llama_embed_media_batch(
        handle: *mut c_void,
        items: *const NativeMediaItem,
        count: c_int,
        out: *mut f32,
        out_stride: c_int,
        err: *mut c_char,
        err_len: c_int,
    ) -> c_int;
    fn ds_llama_embed(
        handle: *mut c_void,
        text: *const c_char,
        is_query: c_int,
        out: *mut f32,
        out_len: c_int,
        err: *mut c_char,
        err_len: c_int,
    ) -> c_int;
    fn ds_llama_embed_batch(
        handle: *mut c_void,
        texts: *const *const c_char,
        count: c_int,
        out: *mut f32,
        out_stride: c_int,
        err: *mut c_char,
        err_len: c_int,
    ) -> c_int;
    fn ds_llama_free(handle: *mut c_void);

    // ---- 后端能力枚举（REQ-093 / REQ-097 / REQ-098 / REQ-100）
    //
    // 需求表里这些能力是按 PyTorch 的 torch.cuda.is_available() 之类写的。
    // 本项目没有 PyTorch：能用的加速后端在编译期就已确定并链进 exe，
    // 所以"检测"= 问已链接的 GGML 后端注册表，而不是在运行时装驱动。
    fn ds_llama_backend_count() -> c_int;
    fn ds_llama_backend_info(
        index: c_int,
        name: *mut c_char,
        name_len: c_int,
        features: *mut c_char,
        features_len: c_int,
        device_count: *mut c_int,
        has_accelerator: *mut c_int,
    ) -> c_int;
    fn ds_llama_device_tier(device_key: *const c_char) -> c_int;
}

/// 一个编译期链入的 GGML 后端（CUDA / Vulkan / Metal / CPU ...）。
///
/// 需求表（REQ-093）把它叫"推理后端"，并按 PyTorch 的写法去探测
/// `torch.cuda.is_available()`。本项目没有 PyTorch：后端是编译期决定的，
/// 运行时装不进来，所以"检测"就是问已链接的后端注册表。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuBackend {
    /// 后端名，例如 `CUDA` / `Vulkan` / `CPU`。
    pub name: String,
    /// 该后端注册的设备数（含 CPU 设备）。
    pub device_count: i32,
    /// 是否至少带一块 GPU / 集成显卡设备。
    pub accelerator: bool,
    /// 编译期特性键值对，例如 `VRAM=8188`。
    pub features: Vec<(String, String)>,
}

/// 枚举链进当前二进制的全部 GGML 后端。
///
/// 纯 CPU 的后端（CPU/BLAS）也会返回：上层要靠它回答"这个安装包到底能不能
/// 用显卡"。若只列出带显卡的后端，纯 CPU 构建会得到空列表，和"枚举失败"
/// 长得一模一样，反而让诊断无从下手。
pub fn gpu_backends() -> Vec<GpuBackend> {
    let count = unsafe { ds_llama_backend_count() };
    if count <= 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut name = [0u8; 128];
        let mut features = [0u8; 1024];
        let mut device_count: c_int = 0;
        let mut accelerator: c_int = 0;
        let ok = unsafe {
            ds_llama_backend_info(
                i,
                name.as_mut_ptr() as *mut c_char,
                name.len() as c_int,
                features.as_mut_ptr() as *mut c_char,
                features.len() as c_int,
                &mut device_count,
                &mut accelerator,
            )
        };
        if ok != 0 {
            continue;
        }
        let name = c_buf_string(&name);
        if name.is_empty() {
            continue;
        }
        let raw_features = c_buf_string(&features);
        let features = raw_features
            .split(';')
            .filter(|item| !item.is_empty())
            .map(|item| match item.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (item.to_string(), String::new()),
            })
            .collect();
        out.push(GpuBackend {
            name,
            device_count,
            accelerator: accelerator != 0,
            features,
        });
    }
    out
}

/// 按显存给设备分档（REQ-097）：0 = 8 GiB 及以上，1 = 4~8 GiB，
/// 2 = 1~4 GiB，3 = 小于 1 GiB（等同纯 CPU）。设备不存在时返回 None。
///
/// 分档阈值与 llama_shim.cpp 里的下放策略必须一致，否则界面报"够用"、
/// 实际却退回 CPU，用户会觉得检测结果是假的。
pub fn device_tier(device_id: &str) -> Option<u8> {
    let key = CString::new(device_id).ok()?;
    let tier = unsafe { ds_llama_device_tier(key.as_ptr()) };
    if tier < 0 {
        None
    } else {
        Some(tier as u8)
    }
}

fn create_handle(
    model_path: *const c_char,
    n_ctx: c_int,
    n_threads: c_int,
    device_name: Option<&CString>,
    err: &mut [u8],
) -> *mut c_void {
    unsafe {
        ds_llama_create(
            model_path,
            n_ctx,
            n_threads,
            device_name.map_or(std::ptr::null(), |s| s.as_ptr()),
            err.as_mut_ptr() as *mut c_char,
            err.len() as c_int,
        )
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuDevice {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub total_memory: u64,
    pub free_memory: u64,
    pub integrated: bool,
    #[serde(skip)]
    physical_id: String,
    /// Backend-native name, for example `CUDA0` or `Vulkan0`. This remains
    /// private because the stable frontend identifier is `id`, but it lets
    /// Candle map the selected CUDA device to its ordinal at runtime.
    #[serde(skip)]
    #[allow(dead_code)]
    backend_name: String,
}

fn c_buf_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim().to_string()
}

/// Enumerate only accelerators exposed by the linked GGML backends. CPU and
/// BLAS devices are intentionally omitted because they cannot satisfy a GPU
/// selection. Entries are deduplicated by PCI id and sorted by total VRAM.
pub fn gpu_devices() -> Vec<GpuDevice> {
    let count = unsafe { ds_llama_device_count() };
    if count <= 0 {
        return Vec::new();
    }
    let mut devices = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut name = [0u8; 256];
        let mut description = [0u8; 512];
        let mut id = [0u8; 256];
        let mut physical = [0u8; 256];
        let mut backend = [0u8; 128];
        let mut free = 0u64;
        let mut total = 0u64;
        let mut device_type: c_int = 0;
        let ok = unsafe {
            ds_llama_device_info(
                i,
                name.as_mut_ptr() as *mut c_char,
                name.len() as c_int,
                description.as_mut_ptr() as *mut c_char,
                description.len() as c_int,
                id.as_mut_ptr() as *mut c_char,
                id.len() as c_int,
                physical.as_mut_ptr() as *mut c_char,
                physical.len() as c_int,
                backend.as_mut_ptr() as *mut c_char,
                backend.len() as c_int,
                &mut free,
                &mut total,
                &mut device_type,
            )
        };
        if ok != 0 {
            continue;
        }
        let name = c_buf_string(&name);
        let description = c_buf_string(&description);
        let id = c_buf_string(&id);
        let physical_id = c_buf_string(&physical);
        let backend = c_buf_string(&backend);
        if id.is_empty() || name.is_empty() {
            continue;
        }
        let backend_name = name.clone();
        let display_name = if description.is_empty() { name } else { description };
        let item = GpuDevice {
            id,
            name: display_name,
            backend,
            total_memory: total,
            free_memory: free,
            integrated: device_type == 1,
            physical_id,
            backend_name,
        };
        if let Some(existing) = devices.iter_mut().find(|x: &&mut GpuDevice| {
            !item.physical_id.is_empty() && x.physical_id == item.physical_id
        }) {
            // One physical card is one user choice. Prefer CUDA when it is
            // available because the safetensors adapter can bind that path;
            // otherwise retain the accelerator with the largest report.
            let item_cuda = item.backend.eq_ignore_ascii_case("cuda");
            let existing_cuda = existing.backend.eq_ignore_ascii_case("cuda");
            if (item_cuda && !existing_cuda) || item.total_memory > existing.total_memory {
                *existing = item;
            }
        } else {
            devices.push(item);
        }
    }
    devices.sort_by(|a, b| b.total_memory.cmp(&a.total_memory).then_with(|| a.name.cmp(&b.name)));
    devices
}

/// Return the CUDA ordinal corresponding to a frontend device id.
///
/// Candle's CUDA API addresses devices by ordinal while the app persists the
/// stable GGML id (`backend|PCI id`). The GGML CUDA backend names devices
/// `CUDA0`, `CUDA1`, ...; using that native name avoids guessing from the
/// display order (which is sorted by VRAM for the UI).
#[cfg(feature = "candle")]
pub fn cuda_device_ordinal(selected_device: &str) -> Result<usize, String> {
    let devices = gpu_devices();
    let selected = if selected_device.trim().is_empty() || selected_device.eq_ignore_ascii_case("auto") {
        devices
            .iter()
            .filter(|d| d.backend.eq_ignore_ascii_case("cuda"))
            .max_by_key(|d| d.total_memory)
    } else {
        devices.iter().find(|d| d.id == selected_device)
    }
    .ok_or_else(|| {
        if selected_device.trim().is_empty() || selected_device.eq_ignore_ascii_case("auto") {
            "未检测到可用 CUDA 显卡；请安装 NVIDIA 驱动并使用带 CUDA 的构建".to_string()
        } else {
            format!("所选 GPU 不可用于 Candle CUDA：{selected_device}")
        }
    })?;

    if !selected.backend.eq_ignore_ascii_case("cuda") {
        return Err(format!(
            "所选 GPU 后端为 {}，EmbeddingGemma2 Candle 只支持 CUDA；请选择 NVIDIA 显卡",
            selected.backend
        ));
    }
    parse_cuda_ordinal(&selected.backend_name).ok_or_else(|| {
        format!(
            "无法从 CUDA 设备标识 {} 解析设备序号，请刷新 GPU 列表",
            selected.backend_name
        )
    })
}

#[cfg(feature = "candle")]
fn parse_cuda_ordinal(name: &str) -> Option<usize> {
    let lower = name.to_ascii_lowercase();
    let suffix = lower.strip_prefix("cuda")?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn error_message(err: &[u8]) -> String {
    unsafe { CStr::from_ptr(err.as_ptr() as *const c_char) }
        .to_string_lossy()
        .to_string()
}

/// 安全包装：Drop 时释放句柄。
struct Handle(*mut c_void);

unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ds_llama_free(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

pub struct LlamaEmbedder {
    text_handle: Mutex<Handle>,
    media_handle: Mutex<Handle>,
    dim: usize,
    context_tokens: usize,
    video_token_budget: usize,
    audio_seconds: f32,
    vision_supported: bool,
    audio_supported: bool,
    model_name: String,
    device: String,
}

fn native_media_error(err: &[u8], fallback: &str) -> String {
    let detail = c_buf_string(err);
    if detail.is_empty() { fallback.to_string() } else { detail }
}

impl LlamaEmbedder {
    /// 加载 GGUF 模型，并要求 llama.cpp 将所有模型层放到选定 GPU。
    pub fn load(
        model_path: &str,
        n_ctx: i32,
        n_threads: i32,
        selected_device: &str,
    ) -> Result<Self, String> {
        Self::load_with_media_config(
            model_path,
            n_ctx,
            n_threads,
            selected_device,
            140,
            30.0,
            1.0,
        )
    }

    /// Load a GGUF embedder with the media budgets selected in the indexing
    /// settings. Keeping this separate from `load` preserves the small public
    /// compatibility API used by older callers while allowing video windows
    /// to be sized against the actual model configuration.
    pub fn load_with_media_config(
        model_path: &str,
        n_ctx: i32,
        n_threads: i32,
        selected_device: &str,
        video_token_budget: usize,
        audio_seconds: f32,
        _video_frame_interval: f32,
    ) -> Result<Self, String> {
        if !std::path::Path::new(model_path).exists() {
            return Err(format!("GGUF 模型不存在：{model_path}"));
        }
        let c_path = CString::new(model_path).map_err(|_| "路径含 NUL 字节".to_string())?;
        // CPU 也是合法的后端：llama.cpp 的 CPU 构建（构建时没有 CUDA/Vulkan
        // 工具链）以及没有可用显卡的机器，如果这里直接报错，调用方就会悄悄退回
        // 哈希基线，语义检索等于废掉。显式选 CPU 或枚举不到加速设备时都走 CPU。
        let selected = if selected_device.eq_ignore_ascii_case("cpu") {
            None
        } else {
            let devices = gpu_devices();
            if devices.is_empty() {
                None
            } else {
                let device = if selected_device.trim().is_empty()
                    || selected_device.eq_ignore_ascii_case("auto")
                {
                    devices.first()
                } else {
                    devices.iter().find(|d| d.id == selected_device)
                }
                .ok_or_else(|| {
                    format!("所选 GPU 不可用或已断开：{selected_device}；请重新选择设备")
                })?;
                Some(device.clone())
            }
        };
        let device_name = selected
            .as_ref()
            .map(|d| CString::new(d.id.as_str()).map_err(|_| "GPU 设备标识包含 NUL 字节"))
            .transpose()?;
        // `0` means automatic in the config. The shim used to translate it
        // to a fixed four threads, which left the CPU underused during
        // tokenisation and prompt preparation on larger machines. Use all
        // available logical CPUs while preserving an explicit user value.
        let effective_threads = if n_threads > 0 {
            n_threads
        } else {
            std::thread::available_parallelism()
                .map(|n| n.get().min(i32::MAX as usize) as i32)
                .unwrap_or(4)
                .max(1)
        };
        let mut err = vec![0u8; 512];
        let media_raw = create_handle(
            c_path.as_ptr(),
            n_ctx,
            effective_threads,
            device_name.as_ref(),
            &mut err,
        );
        if media_raw.is_null() {
            let msg = error_message(&err);
            let target = selected
                .as_ref()
                .map(|d| format!("GPU {} ({})", d.name, d.backend))
                .unwrap_or_else(|| "CPU".into());
            return Err(format!("在 {target} 上加载 GGUF 失败：{msg}"));
        }
        let mut text_err = vec![0u8; 512];
        let text_raw = unsafe {
            ds_llama_duplicate_context(
                media_raw,
                n_ctx,
                effective_threads,
                0,
                text_err.as_mut_ptr() as *mut c_char,
                text_err.len() as c_int,
            )
        };
        if text_raw.is_null() {
            let message = error_message(&text_err);
            unsafe { ds_llama_free(media_raw) };
            return Err(format!("Creating dedicated GGUF text context failed: {message}"));
        }
        let dim = unsafe { ds_llama_dim(media_raw) };
        if dim <= 0 {
            unsafe {
                ds_llama_free(text_raw);
                ds_llama_free(media_raw);
            }
            return Err("模型未报告 embedding 维度".into());
        }
        let capabilities = unsafe { ds_llama_capabilities(media_raw) };
        let name = std::path::Path::new(model_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "gguf".into());
        Ok(Self {
            text_handle: Mutex::new(Handle(text_raw)),
            media_handle: Mutex::new(Handle(media_raw)),
            dim: dim as usize,
            context_tokens: n_ctx.max(256) as usize,
            video_token_budget: video_token_budget.clamp(70, 1120),
            audio_seconds: audio_seconds.clamp(0.25, 30.0),
            vision_supported: capabilities & 2 != 0,
            audio_supported: capabilities & 4 != 0,
            model_name: name,
            device: selected.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "CPU".into()),
        })
    }

    fn embed_one(&self, text: &str, is_query: bool) -> Result<Vec<f32>, String> {
        let c_text = CString::new(text).map_err(|_| "文本含 NUL 字节".to_string())?;
        let mut out = vec![0f32; self.dim];
        let mut err = vec![0u8; 512];
        let guard = self.text_handle.lock().unwrap();
        let n = unsafe {
            ds_llama_embed(
                guard.0,
                c_text.as_ptr(),
                if is_query { 1 } else { 0 },
                out.as_mut_ptr(),
                out.len() as c_int,
                err.as_mut_ptr() as *mut c_char,
                err.len() as c_int,
            )
        };
        if n < 0 {
            let msg = unsafe { CStr::from_ptr(err.as_ptr() as *const c_char) }
                .to_string_lossy()
                .to_string();
            return Err(format!("编码失败：{msg}"));
        }
        let mut v = out;
        l2_normalize(&mut v);
        Ok(v)
    }

    fn embed_native_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let c_texts: Vec<CString> = texts
            .iter()
            .map(|text| CString::new(text.as_str()).map_err(|_| "文本含 NUL 字节".to_string()))
            .collect::<Result<_, _>>()?;
        let pointers: Vec<*const c_char> = c_texts.iter().map(|text| text.as_ptr()).collect();
        let mut output = vec![0.0f32; texts.len() * self.dim];
        let mut err = vec![0u8; 512];
        let guard = self.text_handle.lock().unwrap();
        let n = unsafe {
            ds_llama_embed_batch(
                guard.0,
                pointers.as_ptr(),
                pointers.len() as c_int,
                output.as_mut_ptr(),
                self.dim as c_int,
                err.as_mut_ptr() as *mut c_char,
                err.len() as c_int,
            )
        };
        if n < 0 || n as usize != texts.len() {
            let detail = unsafe { CStr::from_ptr(err.as_ptr() as *const c_char) }
                .to_string_lossy()
                .to_string();
            return Err(if detail.is_empty() { "GGUF 批处理失败".into() } else { detail });
        }
        Ok(output
            .chunks_exact(self.dim)
            .map(|chunk| chunk.to_vec())
            .collect())
    }

    fn embed_native_media(&self, items: &[NativeMediaItem]) -> Result<Vec<f32>, String> {
        if items.is_empty() { return Err("没有可编码的媒体输入".into()); }
        let mut output = vec![0.0f32; self.dim];
        let mut err = vec![0u8; 1024];
        let guard = self.media_handle.lock().map_err(|error| error.to_string())?;
        let n = unsafe {
            ds_llama_embed_media(guard.0, items.as_ptr(), items.len() as c_int,
                output.as_mut_ptr(), output.len() as c_int,
                err.as_mut_ptr() as *mut c_char, err.len() as c_int)
        };
        if n < 0 || n as usize != self.dim {
            return Err(native_media_error(&err, "GGUF mmproj 媒体编码失败"));
        }
        l2_normalize(&mut output);
        Ok(output)
    }

    fn embed_native_media_batch(
        &self,
        images: &[Vec<u8>],
        widths: &[u32],
        heights: &[u32],
        token_budget: c_int,
    ) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() || images.len() != widths.len() || images.len() != heights.len() {
            return Err("GGUF 媒体批次参数不一致".into());
        }
        let items: Vec<NativeMediaItem> = images
            .iter()
            .zip(widths.iter().zip(heights))
            .map(|(data, (&width, &height))| NativeMediaItem {
                kind: 1,
                data: data.as_ptr() as *const c_void,
                length: data.len(),
                width,
                height,
                token_budget,
            })
            .collect();
        let mut output = vec![0.0f32; images.len() * self.dim];
        let mut err = vec![0u8; 1024];
        let guard = self.media_handle.lock().map_err(|error| error.to_string())?;
        let n = unsafe {
            ds_llama_embed_media_batch(
                guard.0,
                items.as_ptr(),
                items.len() as c_int,
                output.as_mut_ptr(),
                self.dim as c_int,
                err.as_mut_ptr() as *mut c_char,
                err.len() as c_int,
            )
        };
        if n < 0 || n as usize != images.len() {
            return Err(native_media_error(&err, "GGUF 图片批量编码失败"));
        }
        Ok(output
            .chunks_exact(self.dim)
            .map(|chunk| {
                let mut vector = chunk.to_vec();
                l2_normalize(&mut vector);
                vector
            })
            .collect())
    }

    fn embed_rgb_batch(
        &self,
        images: &[image::RgbImage],
        token_budget: c_int,
    ) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let data: Vec<Vec<u8>> = images.iter().map(|image| image.as_raw().clone()).collect();
        let widths: Vec<u32> = images.iter().map(|image| image.width()).collect();
        let heights: Vec<u32> = images.iter().map(|image| image.height()).collect();
        self.embed_native_media_batch(&data, &widths, &heights, token_budget)
    }

    fn embed_image_data_refs_batch(
        &self,
        images: &[&image::DynamicImage],
        token_budget: c_int,
    ) -> Result<Vec<Vec<f32>>, String> {
        // The projector and encoder share the model's 8192-token budget. Leave
        // room for the text prefix and avoid oversized mtmd graphs for high
        // resolution standalone images.
        let per_image = (self.context_tokens / (token_budget.max(1) as usize + 32))
            .clamp(1, 32);
        let mut output = Vec::with_capacity(images.len());
        for group in images.chunks(per_image) {
            let rgb: Vec<image::RgbImage> = group
                .par_iter()
                .map(|image| image.to_rgb8())
                .collect();
            output.extend(self.embed_rgb_batch(&rgb, token_budget)?);
        }
        Ok(output)
    }

    fn embed_text_batch_adaptive(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        match self.embed_native_batch(texts) {
            Ok(vectors) => Ok(vectors),
            Err(_) if texts.len() > 1 => {
                let middle = texts.len() / 2;
                let mut first = self.embed_text_batch_adaptive(&texts[..middle])?;
                first.extend(self.embed_text_batch_adaptive(&texts[middle..])?);
                Ok(first)
            }
            Err(_) => texts
                .iter()
                .map(|text| self.embed_one(text, false))
                .collect(),
        }
    }

    fn audio_sample_rate(&self) -> u32 {
        let guard = self.media_handle.lock().unwrap();
        let rate = unsafe { ds_llama_audio_sample_rate(guard.0) };
        if rate > 0 { rate as u32 } else { 16_000 }
    }
}

impl Embedder for LlamaEmbedder {
    fn info(&self) -> EmbedInfo {
        let (capabilities, detail) = {
            let guard = self.media_handle.lock().unwrap();
            let mut buffer = vec![0u8; 512];
            let capabilities = unsafe { ds_llama_capabilities(guard.0) };
            unsafe { ds_llama_detail(guard.0, buffer.as_mut_ptr() as *mut c_char, buffer.len() as c_int) };
            (capabilities, c_buf_string(&buffer))
        };
        let mut modalities = vec!["text".into()];
        if capabilities & 2 != 0 { modalities.push("image".into()); }
        if capabilities & 4 != 0 { modalities.push("audio".into()); }
        if capabilities & 8 != 0 { modalities.push("video".into()); }
        EmbedInfo {
            backend: "llama.cpp".into(),
            model: self.model_name.clone(),
            dim: self.dim,
            device: self.device.clone(),
            // The native mtmd path reports the actual projector capabilities;
            // models without a matching mmproj remain text-only.
            modalities,
            ready: true,
            detail: format!("llama.cpp GGUF · {} · {}", self.device, if detail.is_empty() { "文本" } else { detail.as_str() }),
        }
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_text(&self, text: &str, query: bool) -> Result<Vec<f32>, String> {
        if query {
            return self.embed_one(text, true);
        }
        self.embed_text_batch(&[text.to_owned()])?
            .into_iter()
            .next()
            .ok_or_else(|| "llama.cpp 没有返回文本向量".into())
    }

    fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        const OVERLAP_CHARS: usize = 128;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let max_chars = self.context_tokens.saturating_div(2).max(128);
        let mut chunks_by_text = Vec::with_capacity(texts.len());
        for text in texts {
            let chars: Vec<char> = text.chars().collect();
            let mut chunks = Vec::new();
            if chars.is_empty() {
                chunks.push(String::new());
            } else {
                let mut start = 0usize;
                while start < chars.len() {
                    let end = (start + max_chars).min(chars.len());
                    chunks.push(chars[start..end].iter().collect::<String>());
                    if end == chars.len() {
                        break;
                    }
                    start = end.saturating_sub(OVERLAP_CHARS);
                }
            }
            chunks_by_text.push(chunks);
        }

        let mut flat_chunks = Vec::new();
        let mut owners = Vec::new();
        for (owner, chunks) in chunks_by_text.iter().enumerate() {
            for chunk in chunks {
                owners.push(owner);
                flat_chunks.push(chunk.clone());
            }
        }
        let mut sums = vec![vec![0.0f32; self.dim]; texts.len()];
        let mut counts = vec![0usize; texts.len()];
        let mut start = 0usize;
        while start < flat_chunks.len() {
            let mut end = start;
            let mut estimated_tokens = 0usize;
            while end < flat_chunks.len() && end - start < MAX_NATIVE_BATCH_SEQUENCES {
                let cost = flat_chunks[end].chars().count().saturating_add(32);
                if end > start && estimated_tokens.saturating_add(cost) > self.context_tokens {
                    break;
                }
                estimated_tokens = estimated_tokens.saturating_add(cost);
                end += 1;
            }
            let vectors = self.embed_text_batch_adaptive(&flat_chunks[start..end])?;
            for (offset, vector) in vectors.into_iter().enumerate() {
                let owner = owners[start + offset];
                for (sum, value) in sums[owner].iter_mut().zip(vector) {
                    *sum += value;
                }
                counts[owner] += 1;
            }
            start = end;
        }

        for (sum, count) in sums.iter_mut().zip(counts) {
            if count == 0 {
                return Err("文档没有可编码的文本片段".into());
            }
            for value in &mut *sum {
                *value /= count as f32;
            }
            l2_normalize(sum);
        }
        Ok(sums)
    }

    fn embed_image(&self, path: &std::path::Path) -> Result<Vec<f32>, String> {
        let image = image::open(path).map_err(|error| format!("图片读取失败：{error}"))?;
        self.embed_image_data(&image)
    }

    fn embed_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let rgb = image.to_rgb8();
        let item = NativeMediaItem { kind: 1, data: rgb.as_ptr() as *const c_void,
            length: rgb.len(), width: rgb.width(), height: rgb.height(), token_budget: 1120 };
        self.embed_native_media(std::slice::from_ref(&item))
    }

    fn embed_pdf_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let rgb = image.to_rgb8();
        let item = NativeMediaItem { kind: 1, data: rgb.as_ptr() as *const c_void,
            length: rgb.len(), width: rgb.width(), height: rgb.height(), token_budget: 280 };
        self.embed_native_media(std::slice::from_ref(&item))
    }

    fn embed_image_batch(&self, paths: &[&std::path::Path]) -> Result<Vec<Vec<f32>>, String> {
        let images: Vec<image::DynamicImage> = paths
            .par_iter()
            .map(|path| image::open(path).map_err(|error| format!("图片读取失败：{error}")))
            .collect::<Result<_, _>>()?;
        let refs: Vec<&image::DynamicImage> = images.iter().collect();
        self.embed_image_data_refs_batch(&refs, 1120)
    }

    fn embed_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.embed_image_data_refs_batch(images, 1120)
    }

    fn embed_pdf_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.embed_image_data_refs_batch(images, 280)
    }

    fn embed_audio(&self, path: &std::path::Path) -> Result<Vec<f32>, String> {
        let samples = crate::media::load_audio_at_rate(path, 30.0, self.audio_sample_rate())
            .ok_or("音频解码失败（需要 ffmpeg）")?;
        let item = NativeMediaItem { kind: 2, data: samples.as_ptr() as *const c_void,
            length: samples.len(), width: 0, height: 0, token_budget: 0 };
        self.embed_native_media(std::slice::from_ref(&item))
    }

    fn embed_video(&self, path: &std::path::Path) -> Result<Vec<f32>, String> {
        let vectors = self.embed_video_segments(path)?;
        let mut merged = vec![0.0f32; self.dim];
        for vector in &vectors { for (dst, value) in merged.iter_mut().zip(vector) { *dst += value; } }
        let count = vectors.len() as f32;
        for value in &mut merged { *value /= count; }
        l2_normalize(&mut merged);
        Ok(merged)
    }

    fn embed_video_segments(&self, path: &std::path::Path) -> Result<Vec<Vec<f32>>, String> {
        let duration = crate::media::video_duration(path).unwrap_or(0.0).max(0.1);
        // A 30-second window is only valid when its projected frames and
        // audio fit the shared model context.  Older configs commonly use
        // 4096 tokens, so dynamically shorten the window instead of sending
        // an oversized sequence that makes the entire video fail.
        let window_seconds = crate::media::safe_video_window_seconds(
            self.context_tokens,
            self.video_token_budget,
            self.audio_supported,
            self.audio_seconds,
        );
        let mut vectors = Vec::new();
        let mut start = 0.0f64;
        while start < duration {
            let window = (duration - start).min(window_seconds);
            let frame_paths = if self.vision_supported {
                crate::media::sample_video_frames_window(path, start, window, 1024)
            } else {
                Vec::new()
            };
            let mut images = Vec::new();
            for frame in &frame_paths {
                if let Ok(image) = image::open(frame) { images.push(image.to_rgb8()); }
                let _ = std::fs::remove_file(frame);
            }
            if let Some(parent) = frame_paths.first().and_then(|frame| frame.parent()) { let _ = std::fs::remove_dir_all(parent); }
            let audio = if self.audio_supported {
                crate::media::load_audio_segment_at_rate(
                    path,
                    start,
                    window,
                    self.audio_sample_rate(),
                )
            } else {
                None
            };
            let mut items = Vec::with_capacity(images.len() + usize::from(audio.is_some()));
            for image in &images {
                items.push(NativeMediaItem { kind: 1, data: image.as_ptr() as *const c_void,
                    length: image.len(), width: image.width(), height: image.height(), token_budget: 140 });
            }
            if let Some(samples) = audio.as_ref() {
                items.push(NativeMediaItem { kind: 2, data: samples.as_ptr() as *const c_void,
                    length: samples.len(), width: 0, height: 0, token_budget: 0 });
            }
            if !items.is_empty() { vectors.push(self.embed_native_media(&items)?); }
            start += window;
        }
        if vectors.is_empty() { return Err("视频未能解码出图像帧或音频轨".into()); }
        Ok(vectors)
    }

}

#[cfg(test)]
mod tests {
    use super::gpu_devices;

    #[test]
    fn accelerator_devices_are_named_and_sorted_by_memory() {
        let devices = gpu_devices();
        assert!(devices.iter().all(|device| !device.name.is_empty() && !device.id.is_empty()));
        assert!(devices.windows(2).all(|pair| pair[0].total_memory >= pair[1].total_memory));
        for device in devices {
            println!(
                "GPU: {} | {} | total={} free={}",
                device.name, device.backend, device.total_memory, device.free_memory
            );
        }
    }
}
