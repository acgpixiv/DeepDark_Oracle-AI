//! 向量化抽象层。
//!
//! - `LlamaEmbedder`：通过 llama.cpp 的 C API 做文本/图片 embedding（真模型）
//! - `HashEmbedder`：无模型时的确定性兜底（文本 n-gram + 图片颜色直方图 + 音视频字节特征）
pub mod bpe;
pub mod clap;
pub mod clip;
/// GPU 能力探测与后端选择（REQ-093 ~ REQ-102）。无 GPU 时全部降级为 CPU，
/// 不返回错误 —— GPU 检测失败绝不能让主程序起不来。
pub mod gpu;
pub mod hash;
#[cfg(feature = "candle")]
pub mod candle_backend;
#[cfg(feature = "llama")]
pub mod llama;
pub mod ort;
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbedInfo {
    pub backend: String,
    pub model: String,
    pub dim: usize,
    pub device: String,
    pub modalities: Vec<String>,
    pub ready: bool,
    pub detail: String,
}

pub trait Embedder: Send + Sync {
    fn info(&self) -> EmbedInfo;
    fn dim(&self) -> usize;
    fn concurrency(&self) -> usize { 1 }
    /// 文本向量；`query=true` 时加检索指令前缀。
    fn embed_text(&self, text: &str, query: bool) -> Result<Vec<f32>, String>;
    /// Batch document encoding. Backends that support native batching should
    /// override this; the default preserves compatibility with older models.
    fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        texts.iter().map(|text| self.embed_text(text, false)).collect()
    }
    fn embed_image_batch(&self, paths: &[&Path]) -> Result<Vec<Vec<f32>>, String> {
        paths.iter().map(|path| self.embed_image(path)).collect()
    }
    fn embed_image(&self, path: &Path) -> Result<Vec<f32>, String>;
    fn embed_pdf_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        self.embed_image_data(image)
    }
    fn embed_image_data(&self, _image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        Err("当前模型不支持内存图像编码".into())
    }
    fn embed_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        images.iter().map(|image| self.embed_image_data(image)).collect()
    }
    fn embed_pdf_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        images.iter().map(|image| self.embed_pdf_image_data(image)).collect()
    }
    fn embed_audio(&self, path: &Path) -> Result<Vec<f32>, String>;
    fn embed_video(&self, path: &Path) -> Result<Vec<f32>, String>;
    fn video_window_vectors(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        self.embed_video(path).map(|vector| vec![vector])
    }
    /// Return one vector per bounded video window. Legacy backends expose a
    /// single file vector through the default implementation; multimodal
    /// backends can retain temporal locality by overriding this method.
    fn embed_video_segments(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        self.video_window_vectors(path)
    }
}
/// IDs used for temporal video vectors in the existing file-keyed store.
/// SQLite file IDs are positive; negative IDs therefore remain backwards
/// compatible while allowing several 768-D windows to point at one file.
const VIDEO_SEGMENT_ID_SCALE: i64 = 1_000_000;

pub fn video_segment_id(file_id: i64, segment: usize) -> i64 {
    -(file_id.saturating_mul(VIDEO_SEGMENT_ID_SCALE)
        .saturating_add(segment as i64 + 1))
}

pub fn video_segment_source(id: i64) -> Option<i64> {
    if id >= 0 {
        return None;
    }
    let encoded = id.checked_neg()?;
    let source = encoded / VIDEO_SEGMENT_ID_SCALE;
    let segment = encoded % VIDEO_SEGMENT_ID_SCALE;
    (source > 0 && segment > 0).then_some(source)
}

/// 按文件类型分发到对应模态。
pub fn embed_path(e: &dyn Embedder, path: &Path, kind: &str) -> Result<Vec<f32>, String> {
    if path.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("pdf")).unwrap_or(false) {
        let text = crate::textract::extract_text(path, 20_000).unwrap_or_default();
        let images = crate::textract::pdf_images(path, 8);
        return embed_pdf_parts(e, (!text.trim().is_empty()).then_some(text.as_str()), &images);
    }
    match kind {
        "image" => e.embed_image(path),
        "audio" => e.embed_audio(path),
        "video" => e.embed_video(path),
        _ => {
            let text = crate::textract::extract_text(path, 20_000).unwrap_or_default();
            if text.trim().is_empty() {
                return Err("无文本内容".into());
            }
            e.embed_text(&text, false)
        }
    }
}

/// Encode the pieces already extracted from a PDF and fuse them into one
/// document vector. Callers can pass a cached extraction so a PDF is unpacked
/// only once per indexing item.
pub fn embed_pdf_parts(
    e: &dyn Embedder,
    text: Option<&str>,
    images: &[image::DynamicImage],
) -> Result<Vec<f32>, String> {
    let mut vectors = Vec::new();
    if let Some(text) = text.filter(|value| !value.trim().is_empty()) {
        vectors.push(e.embed_text(text, false)?);
    }
    if !images.is_empty() {
        let image_refs: Vec<&image::DynamicImage> = images.iter().collect();
        match e.embed_pdf_image_data_batch(&image_refs) {
            Ok(encoded) => vectors.extend(encoded),
            Err(_) => {
                // A single malformed/unsupported raster should not discard
                // the other page images. Retry per image only on the rare
                // batch failure, keeping the normal path fully batched.
                for image in images {
                    if let Ok(vector) = e.embed_pdf_image_data(image) {
                        vectors.push(vector);
                    }
                }
            }
        }
    }
    if vectors.is_empty() {
        return Err("PDF 未抽取到可用文字或图片".into());
    }
    fuse_vectors(vectors, "PDF 编码结果为空")
}

fn fuse_vectors(vectors: Vec<Vec<f32>>, empty_message: &str) -> Result<Vec<f32>, String> {
    let Some(first) = vectors.first() else {
        return Err(empty_message.into());
    };
    let mut merged = vec![0.0f32; first.len()];
    let mut count = 0.0f32;
    for vector in vectors {
        if vector.len() != merged.len() {
            continue;
        }
        for (dst, value) in merged.iter_mut().zip(vector) {
            *dst += value;
        }
        count += 1.0;
    }
    if count == 0.0 {
        return Err(empty_message.into());
    }
    for value in &mut merged {
        *value /= count;
    }
    l2_normalize(&mut merged);
    Ok(merged)
}

/// Encode several PDFs in one operation. Texts and page images are flattened
/// into batch calls, then fused back into one vector per source document.
pub fn embed_pdf_parts_batch(
    e: &dyn Embedder,
    parts: &[(Option<String>, &[image::DynamicImage])],
) -> Vec<Result<Vec<f32>, String>> {
    let mut output: Vec<Option<Result<Vec<f32>, String>>> = vec![None; parts.len()];
    let result = embed_pdf_parts_batch_with(e, parts, || false, |index, vector| {
        output[index] = Some(vector);
    });
    if let Err(error) = result {
        return (0..parts.len()).map(|_| Err(error.clone())).collect();
    }
    output
        .into_iter()
        .map(|value| value.unwrap_or_else(|| Err("PDF 向量编码未完成".into())))
        .collect()
}

/// Encode bounded chunks and report each PDF once all of its page images are
/// encoded. The caller's cancellation hook is checked between model calls.
pub fn embed_pdf_parts_batch_with<F, C>(
    e: &dyn Embedder,
    parts: &[(Option<String>, &[image::DynamicImage])],
    cancelled: C,
    mut on_document: F,
) -> Result<(), String>
where
    F: FnMut(usize, Result<Vec<f32>, String>),
    C: Fn() -> bool + Sync,
{
    // The backend still applies its own token/attention budget and splits
    // oversized calls. A 16-item outer chunk gives both llama.cpp and Candle
    // enough work to keep the GPU fed, while bounding the number of decoded
    // PDF rasters retained by this routine.
    const PDF_EMBED_CHUNK: usize = 16;
    let mut text_map = Vec::new();
    let mut texts = Vec::new();
    let mut image_map = Vec::new();
    let mut images = Vec::new();
    for (index, (text, document_images)) in parts.iter().enumerate() {
        if let Some(text) = text.as_deref().filter(|value| !value.trim().is_empty()) {
            text_map.push(index);
            texts.push(text.to_owned());
        }
        for image in document_images.iter() {
            image_map.push(index);
            images.push(image);
        }
    }
    let encode_text_lane = || -> Result<Vec<Option<Vec<f32>>>, String> {
        let mut encoded = Vec::with_capacity(texts.len());
        for text_chunk in texts.chunks(PDF_EMBED_CHUNK) {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            match e.embed_text_batch(text_chunk) {
                Ok(rows) if rows.len() == text_chunk.len() => {
                    encoded.extend(rows.into_iter().map(Some));
                }
                _ => {
                    for text in text_chunk {
                        if cancelled() {
                            return Err("index cancellation requested".into());
                        }
                        encoded.push(e.embed_text(text, false).ok());
                    }
                }
            }
        }
        Ok(encoded)
    };
    let encode_image_lane = || -> Result<Vec<Option<Vec<f32>>>, String> {
        let mut encoded = Vec::with_capacity(images.len());
        for image_chunk in images.chunks(PDF_EMBED_CHUNK) {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            match e.embed_pdf_image_data_batch(image_chunk) {
                Ok(rows) if rows.len() == image_chunk.len() => {
                    encoded.extend(rows.into_iter().map(Some));
                }
                _ => {
                    for image in image_chunk {
                        if cancelled() {
                            return Err("index cancellation requested".into());
                        }
                        encoded.push(e.embed_pdf_image_data(image).ok());
                    }
                }
            }
        }
        Ok(encoded)
    };

    // GGUF has distinct text and mtmd contexts. Let those lanes overlap;
    // Candle and legacy adapters keep their established serial scheduling.
    let dual_context = e.info().backend == "llama.cpp" && !texts.is_empty() && !images.is_empty();
    let (encoded_texts, encoded_images) = if dual_context {
        std::thread::scope(|scope| {
            let text_worker = scope.spawn(encode_text_lane);
            let image_result = encode_image_lane();
            let text_result = text_worker
                .join()
                .map_err(|_| "PDF text embedding worker panicked".to_string())?;
            Ok::<_, String>((text_result?, image_result?))
        })?
    } else {
        (encode_text_lane()?, encode_image_lane()?)
    };
    if cancelled() {
        return Err("index cancellation requested".into());
    }

    let mut per_document: Vec<Vec<Vec<f32>>> = vec![Vec::new(); parts.len()];
    for (index, vector) in text_map.into_iter().zip(encoded_texts) {
        if let Some(vector) = vector {
            per_document[index].push(vector);
        }
    }
    for (index, vector) in image_map.into_iter().zip(encoded_images) {
        if let Some(vector) = vector {
            per_document[index].push(vector);
        }
    }
    for (index, vectors) in per_document.into_iter().enumerate() {
        on_document(index, fuse_vectors(vectors, "PDF 未抽取到可用文字或图片"));
    }
    Ok(())
}

pub fn l2_normalize(v: &mut [f32]) {
    let mut sum = 0.0f32;
    for x in v.iter() {
        sum += x * x;
    }
    let n = sum.sqrt();
    if n > 1e-8 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Resolve a user-selected model file or folder to the actual embedding
/// entrypoint. Model downloads often contain both a safetensors package and a
/// GGUF `mmproj`; the projector is a companion, never a standalone embedder.
pub fn resolve_model_path(input: &str) -> Result<std::path::PathBuf, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("模型路径为空".into());
    }
    let path = Path::new(raw);
    if path.is_file() {
        let ext = path.extension().and_then(|x| x.to_str()).unwrap_or_default();
        if ext.eq_ignore_ascii_case("gguf") {
            let name = path.file_name().and_then(|x| x.to_str()).unwrap_or_default().to_ascii_lowercase();
            if name.contains("mmproj") || name.contains("projector") {
                return Err("mmproj 是投影配套文件，不能作为独立嵌入模型加载".into());
            }
            return Ok(path.to_path_buf());
        }
        if ext.eq_ignore_ascii_case("safetensors") {
            return Ok(path.to_path_buf());
        }
        return Err(format!("无法识别模型格式：{raw}"));
    }
    if !path.is_dir() {
        return Err(format!("模型路径不存在：{raw}"));
    }

    let mut safe_dirs = Vec::new();
    let mut ggufs = Vec::new();
    for entry in walkdir::WalkDir::new(path).follow_links(false).max_depth(4).into_iter().flatten() {
        let p = entry.path();
        if !p.is_file() { continue; }
        let name = p.file_name().and_then(|x| x.to_str()).unwrap_or_default().to_ascii_lowercase();
        let is_safetensors_weight = name == "model.safetensors"
            || name == "model.safetensors.index.json"
            || (name.ends_with(".safetensors") && name.contains("-of-"));
        let arbitrary_single_weight = p.extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
            && p.parent().is_some_and(has_single_safetensors_file);
        if (is_safetensors_weight || arbitrary_single_weight)
            && p.parent().is_some_and(safetensors_package_complete)
        {
            safe_dirs.push(p.parent().unwrap().to_path_buf());
        }
        if p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("gguf")).unwrap_or(false)
            && !name.contains("mmproj") && !name.contains("projector")
        {
            ggufs.push(p.to_path_buf());
        }
    }
    safe_dirs.sort();
    safe_dirs.dedup();
    if let Some(dir) = safe_dirs.into_iter().next() {
        return Ok(dir);
    }
    ggufs.sort_by_key(|p| std::cmp::Reverse(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)));
    ggufs.into_iter().next().ok_or_else(|| format!("目录中没有找到完整的 safetensors 或 GGUF 嵌入模型：{raw}"))
}

fn has_single_safetensors_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
        {
            count += 1;
            if count > 1 {
                return false;
            }
        }
    }
    count == 1
}

pub(crate) fn safetensors_package_complete(dir: &Path) -> bool {
    if !dir.join("config.json").is_file()
        || !(dir.join("tokenizer.json").is_file() || dir.join("tokenizer.model").is_file())
    {
        return false;
    }
    let index = dir.join("model.safetensors.index.json");
    if index.is_file() {
        let Ok(raw) = std::fs::read_to_string(index) else { return false };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
        let Some(weights) = value.get("weight_map").and_then(serde_json::Value::as_object) else {
            return false;
        };
        return !weights.is_empty()
            && weights.values().all(|file| {
                file.as_str().is_some_and(|name| dir.join(name).is_file())
            });
    }
    dir.join("model.safetensors").is_file() || has_single_safetensors_file(dir)
}

/// The in-tree Candle adapter is intentionally specific to EmbeddingGemma2.
/// Other safetensors embedding families (for example Qwen3-Embedding and
/// Jina v5 Omni) need their own graph/runtime or a Python/ONNX adapter; do not
/// send them through the EmbeddingGemma loader and report a misleading tensor
/// error.
pub fn is_embedding_gemma_dir(dir: &Path) -> bool {
    let Ok(config) = std::fs::read_to_string(dir.join("config.json")) else {
        return false;
    };
    let lower = config.to_ascii_lowercase();
    lower.contains("embeddinggemma2model") || lower.contains("embedding_gemma2")
}

/// 按配置构建 embedding 后端。
///
/// `embed_model` 根据模型格式选择可用的原生适配器：GGUF 使用 llama.cpp，
/// EmbeddingGemma2 safetensors 在启用 candle feature 时使用 Candle；未知格式
/// 不会被强行交给错误后端。
pub fn build_embedder(cfg: &crate::config::ModelConfig) -> Result<Box<dyn Embedder>, String> {
    build_embedder_with_batch(cfg, 8)
}

/// Pick a throughput-oriented batch size for the selected indexing profile.
///
/// `Options::batch_size` is the batch size used inside the Candle model.  The
/// old code only changed the queue batch size, while Candle kept its default
/// eight-item batches, so a safetensors model could leave a capable GPU mostly
/// idle.  Keep the values bounded to avoid turning a large document set into
/// an uncontrolled VRAM allocation; Candle still splits long sequences by
/// attention cost.
pub fn effective_batch_size(cfg: &crate::config::ModelConfig, configured: usize) -> usize {
    let requested = configured.clamp(1, 128);
    let cpu_workers = effective_parallel_threads(cfg);
    let gpu_ceiling = selected_gpu_batch_ceiling(cfg);
    requested
        .max(cpu_workers.saturating_mul(2))
        .clamp(1, gpu_ceiling)
}

/// Attention is evaluated in bounded query chunks inside the Candle adapter.
/// A GPU with ample free VRAM can therefore safely keep more independent
/// image/text sequences in one batch, while smaller or busy GPUs retain the
/// conservative budget used by older builds.
pub fn attention_score_budget(selected_gpu: &str) -> usize {
    #[cfg(feature = "llama")]
    {
        let devices = llama::gpu_devices();
        let selected = if selected_gpu.trim().is_empty()
            || selected_gpu.eq_ignore_ascii_case("auto")
        {
            devices.iter().max_by_key(|device| device.free_memory)
        } else {
            devices.iter().find(|device| device.id == selected_gpu)
        };
        if let Some(device) = selected {
            return attention_score_budget_for_free_vram(device.free_memory);
        }
    }
    1usize << 28
}

fn attention_score_budget_for_free_vram(free_bytes: u64) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    if free_bytes >= 10 * GIB {
        1usize << 32
    } else if free_bytes >= 6 * GIB {
        1usize << 31
    } else if free_bytes >= 3 * GIB {
        1usize << 30
    } else {
        1usize << 28
    }
}

fn batch_ceiling_for_free_vram(free_bytes: u64) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    if free_bytes >= 8 * GIB {
        64
    } else if free_bytes >= 4 * GIB {
        32
    } else if free_bytes >= 2 * GIB {
        16
    } else {
        8
    }
}

fn selected_gpu_batch_ceiling(cfg: &crate::config::ModelConfig) -> usize {
    #[cfg(feature = "llama")]
    {
        let devices = llama::gpu_devices();
        let selected = if cfg.gpu_device.trim().is_empty()
            || cfg.gpu_device.eq_ignore_ascii_case("auto")
        {
            devices.iter().max_by_key(|device| device.free_memory)
        } else {
            devices.iter().find(|device| device.id == cfg.gpu_device)
        };
        if let Some(device) = selected {
            if device.free_memory == 0 {
                return 16;
            }
            return batch_ceiling_for_free_vram(device.free_memory);
        }
    }
    // No reliable accelerator memory report: keep batches moderate. The
    // model-specific attention/token limits still apply inside each backend.
    32
}

fn logical_cpu_threads() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .max(1);
    logical
}

#[cfg(windows)]
fn windows_cpu_idle_fraction() -> Option<f64> {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn value(self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    }

    let mut idle_before = FileTime::default();
    let mut kernel_before = FileTime::default();
    let mut user_before = FileTime::default();
    if unsafe { GetSystemTimes(&mut idle_before, &mut kernel_before, &mut user_before) } == 0 {
        return None;
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    let mut idle_after = FileTime::default();
    let mut kernel_after = FileTime::default();
    let mut user_after = FileTime::default();
    if unsafe { GetSystemTimes(&mut idle_after, &mut kernel_after, &mut user_after) } == 0 {
        return None;
    }
    let idle = idle_after.value().saturating_sub(idle_before.value());
    let kernel = kernel_after.value().saturating_sub(kernel_before.value());
    let user = user_after.value().saturating_sub(user_before.value());
    let total = kernel.saturating_add(user);
    (total > 0).then(|| (idle as f64 / total as f64).clamp(0.0, 1.0))
}

#[cfg(feature = "llama")]
fn effective_model_threads(cfg: &crate::config::ModelConfig) -> i32 {
    effective_parallel_threads(cfg).min(i32::MAX as usize) as i32
}
/// Single-model host scheduling policy. Automatic mode samples current CPU
/// headroom on Windows, then caps indexing at 90% of logical processors.
pub fn effective_parallel_threads(cfg: &crate::config::ModelConfig) -> usize {
    let cores = logical_cpu_threads();
    let available = {
        #[cfg(windows)]
        {
            windows_cpu_idle_fraction()
                .map(|idle| ((cores as f64 * idle).floor() as usize).clamp(1, cores))
                .unwrap_or(cores)
        }
        #[cfg(not(windows))]
        {
            cores
        }
    };
    match cfg.parallel_mode.trim().to_ascii_lowercase().as_str() {
        "eco" => cores.div_ceil(3).min(available).max(1),
        "custom" => cfg.parallel_threads.clamp(1, cores),
        _ => ((cores as f64 * 0.90).floor() as usize).min(available).max(1),
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    struct NumberEmbedder {
        offset: f32,
    }

    impl Embedder for NumberEmbedder {
        fn info(&self) -> EmbedInfo {
            EmbedInfo {
                backend: "test".into(),
                model: "test".into(),
                dim: 1,
                device: "test".into(),
                modalities: vec!["text".into()],
                ready: true,
                detail: String::new(),
            }
        }

        fn dim(&self) -> usize { 1 }

        fn embed_text(&self, text: &str, _query: bool) -> Result<Vec<f32>, String> {
            Ok(vec![text.parse::<f32>().map_err(|e| e.to_string())? + self.offset])
        }

        fn embed_image(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }
        fn embed_audio(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }
        fn embed_video(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }
    }

    struct DualLaneEmbedder {
        active: std::sync::atomic::AtomicUsize,
        overlapped: std::sync::atomic::AtomicBool,
    }

    impl DualLaneEmbedder {
        fn work(&self) {
            use std::sync::atomic::Ordering;
            if self.active.fetch_add(1, Ordering::SeqCst) > 0 {
                self.overlapped.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Embedder for DualLaneEmbedder {
        fn info(&self) -> EmbedInfo {
            EmbedInfo {
                backend: "llama.cpp".into(),
                model: "test".into(),
                dim: 1,
                device: "test".into(),
                modalities: vec!["text".into(), "image".into()],
                ready: true,
                detail: String::new(),
            }
        }

        fn dim(&self) -> usize { 1 }

        fn embed_text(&self, _text: &str, _query: bool) -> Result<Vec<f32>, String> {
            self.work();
            Ok(vec![1.0])
        }

        fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.work();
            Ok(vec![vec![1.0]; texts.len()])
        }

        fn embed_image(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }

        fn embed_pdf_image_data_batch(
            &self,
            images: &[&image::DynamicImage],
        ) -> Result<Vec<Vec<f32>>, String> {
            self.work();
            Ok(vec![vec![3.0]; images.len()])
        }

        fn embed_audio(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }
        fn embed_video(&self, _path: &Path) -> Result<Vec<f32>, String> { Err("unused".into()) }
    }

    #[test]
    fn pdf_batch_fallback_keeps_each_document_result_in_order() {
        let embedder = NumberEmbedder { offset: 0.0 };
        let no_images: &[image::DynamicImage] = &[];
        let parts = [
            (Some("invalid".to_owned()), no_images),
            (Some("2".to_owned()), no_images),
        ];

        let vectors = embed_pdf_parts_batch(&embedder, &parts);
        assert!(vectors[0].is_err());
        assert_eq!(vectors[1].as_ref().unwrap(), &vec![1.0]);
    }

    #[test]
    fn llama_pdf_text_and_image_lanes_overlap_and_fuse() {
        use std::sync::atomic::Ordering;

        let embedder = DualLaneEmbedder {
            active: std::sync::atomic::AtomicUsize::new(0),
            overlapped: std::sync::atomic::AtomicBool::new(false),
        };
        let image = image::DynamicImage::new_rgb8(2, 2);
        let parts = [(Some("page text".to_owned()), std::slice::from_ref(&image))];

        let vectors = embed_pdf_parts_batch(&embedder, &parts);
        assert_eq!(vectors[0].as_ref().unwrap(), &vec![1.0]);
        assert!(embedder.overlapped.load(Ordering::SeqCst));
    }

    #[test]
    fn gpu_batch_ceiling_reserves_capacity_as_free_vram_drops() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(batch_ceiling_for_free_vram(12 * GIB), 64);
        assert_eq!(batch_ceiling_for_free_vram(6 * GIB), 32);
        assert_eq!(batch_ceiling_for_free_vram(3 * GIB), 16);
        assert_eq!(batch_ceiling_for_free_vram(GIB), 8);
    }

    #[test]
    fn attention_batch_budget_tracks_free_gpu_memory() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(attention_score_budget_for_free_vram(12 * GIB), 1usize << 32);
        assert_eq!(attention_score_budget_for_free_vram(8 * GIB), 1usize << 31);
        assert_eq!(attention_score_budget_for_free_vram(4 * GIB), 1usize << 30);
        assert_eq!(attention_score_budget_for_free_vram(GIB), 1usize << 28);
    }

    #[test]
    fn model_directory_resolution_prefers_complete_safetensors_package() {
        let dir = std::env::temp_dir().join(format!(
            "deepaisee-sharded-model-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.model"), b"tokenizer").unwrap();
        std::fs::write(dir.join("model-00001-of-00001.safetensors"), b"weights").unwrap();
        std::fs::write(
            dir.join("model.safetensors.index.json"),
            r#"{"weight_map":{"a":"model-00001-of-00001.safetensors"}}"#,
        )
        .unwrap();

        assert_eq!(resolve_model_path(dir.to_str().unwrap()).unwrap(), dir);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn model_directory_resolution_accepts_a_custom_single_safetensors_name() {
        let dir = std::env::temp_dir().join(format!(
            "deepaisee-custom-model-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        std::fs::write(dir.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.join("embeddinggemma-2.safetensors"), b"weights").unwrap();

        assert_eq!(resolve_model_path(dir.to_str().unwrap()).unwrap(), dir);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

pub fn build_embedder_with_batch(
    cfg: &crate::config::ModelConfig,
    batch_size: usize,
) -> Result<Box<dyn Embedder>, String> {
    build_embedder_with_index(cfg, &crate::config::IndexConfig::default(), batch_size)
}

/// Build an embedder with the complete indexing configuration. The legacy
/// helper above remains for CLI and compatibility callers that do not carry
/// an `AppConfig`.
pub fn build_embedder_with_index(
    cfg: &crate::config::ModelConfig,
    index: &crate::config::IndexConfig,
    batch_size: usize,
) -> Result<Box<dyn Embedder>, String> {
    #[cfg(not(feature = "candle"))]
    let _ = (batch_size, index);
    let path = cfg.embed_model.trim();
    if path.is_empty() {
        return Ok(Box::new(hash::HashEmbedder::new()));
    }
    let resolved = resolve_model_path(path)?;
    let model_path = resolved.as_path();
    let safe_dir = if model_path.is_dir() {
        Some(model_path)
    } else if model_path.extension().map(|x| x.eq_ignore_ascii_case("safetensors")).unwrap_or(false) {
        model_path.parent()
    } else {
        None
    };
    if let Some(_dir) = safe_dir.filter(|p| p.exists()) {
        if !is_embedding_gemma_dir(_dir) {
            return Err(format!(
                "当前原生 Candle 适配器只支持 EmbeddingGemma2 safetensors：{}；Qwen3-Embedding/Jina v5 Omni 请导入对应 GGUF，或安装 Python/ONNX 扩展",
                _dir.display()
            ));
        }
        #[cfg(feature = "candle")]
        {
            return candle_backend::CandleEmbedder::load(
                _dir,
                &cfg.gpu_device,
                effective_batch_size(cfg, batch_size),
                cfg.context_size as usize,
                index.image_token_budget,
                index.video_token_budget,
                index.vision_encoder,
                index.audio_encoder,
                index.video_index_audio,
                index.audio_seconds,
                index.video_frame_interval,
                index.video_max_frames,
                index.image_max_side,
                index.pdf_image_token_budget,
            ).map(|e| Box::new(e) as Box<dyn Embedder>);
        }
        #[cfg(not(feature = "candle"))]
        return Err("此 safetensors 模型需要使用 --features candle 构建原生 Candle 后端".into());
    }
    #[cfg(feature = "llama")]
    {
        if model_path.extension().map(|x| x.eq_ignore_ascii_case("gguf")).unwrap_or(false) {
            return match llama::LlamaEmbedder::load_with_media_config(
                &resolved.to_string_lossy(),
                cfg.context_size as i32,
                effective_model_threads(cfg),
                &cfg.gpu_device,
                index.video_token_budget,
                index.audio_seconds,
                index.video_frame_interval,
            ) {
                Ok(e) => Ok(Box::new(e)),
                Err(e) => Err(format!("llama.cpp 加载失败：{e}")),
            };
        }
        return Err(format!("模型文件不存在或没有匹配的原生后端：{path}"));
    }
    #[cfg(not(feature = "llama"))]
    {
        let _ = path;
        Ok(Box::new(hash::HashEmbedder::new()))
    }
}
