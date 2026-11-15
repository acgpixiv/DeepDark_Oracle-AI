//! 向量化抽象层。
//!
//! - `LlamaEmbedder`：通过 llama.cpp 的 C API 做文本/图片 embedding（真模型）
//! - `HashEmbedder`：无模型时的确定性兜底（文本 n-gram + 图片颜色直方图 + 音视频字节特征）
pub mod bpe;
#[cfg(feature = "candle")]
pub mod candle_backend;
pub mod clap;
pub mod clip;
/// GPU 能力探测与后端选择（REQ-093 ~ REQ-102）。无 GPU 时全部降级为 CPU，
/// 不返回错误 —— GPU 检测失败绝不能让主程序起不来。
pub mod gpu;
pub mod hash;
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
    fn concurrency(&self) -> usize {
        1
    }
    /// 文本向量；`query=true` 时加检索指令前缀。
    fn embed_text(&self, text: &str, query: bool) -> Result<Vec<f32>, String>;
    /// Batch document encoding. Backends that support native batching should
    /// override this; the default preserves compatibility with older models.
    fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        texts
            .iter()
            .map(|text| self.embed_text(text, false))
            .collect()
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
        images
            .iter()
            .map(|image| self.embed_image_data(image))
            .collect()
    }
    fn embed_pdf_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        images
            .iter()
            .map(|image| self.embed_pdf_image_data(image))
            .collect()
    }
    /// Encode video frames with the backend's configured frame budget.
    /// Backends without a separate video-frame implementation use the
    /// conservative PDF-image path for compatibility.
    fn embed_video_frame_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.embed_pdf_image_data_batch(images)
    }
    fn embed_audio(&self, path: &Path) -> Result<Vec<f32>, String>;
    /// Encode one vector per bounded audio window.
    ///
    /// Built-in audio backends override this method so long recordings are
    /// decoded with the projector's native PCM sample rate. Third-party
    /// implementations keep the old one-vector behavior by default.
    fn embed_audio_segments(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        self.embed_audio(path).map(|vector| vec![vector])
    }
    /// Encode an entire audio file while retaining the existing file-keyed
    /// vector schema. Window vectors are fused into one normalized vector.
    fn embed_audio_full(&self, path: &Path) -> Result<Vec<f32>, String> {
        let vectors = self.embed_audio_segments(path)?;
        fuse_vectors(vectors, "闊抽鏈繑鍥炲悜閲?")
    }
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
    /// Return temporally addressable video embeddings. Backends that cannot
    /// expose temporal metadata retain the legacy vectors and receive evenly
    /// spaced windows as a compatibility fallback.
    fn embed_video_segment_details(
        &self,
        path: &Path,
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        let vectors = self.embed_video_segments(path)?;
        if vectors.is_empty() {
            return Err("video backend returned no segment vectors".into());
        }
        let duration = crate::media::require_video_duration(path)?;
        let window = (duration / vectors.len().max(1) as f64).max(0.1);
        Ok(vectors
            .into_iter()
            .enumerate()
            .map(|(index, vector)| VideoSegmentEmbedding {
                segment_no: index,
                start_seconds: index as f64 * window,
                end_seconds: ((index + 1) as f64 * window).min(duration),
                subtitle: String::new(),
                vector,
            })
            .collect())
    }
    /// Cancellation-aware variant used by long indexing jobs. Backends with
    /// windowed decoding should override this so a stop request is observed
    /// between windows; legacy implementations retain their existing behavior.
    fn embed_video_segment_details_with_cancel(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        if cancelled() {
            return Err("index cancellation requested".into());
        }
        self.embed_video_segment_details(path)
    }

    /// Cancellation-aware video encoding with transient segment progress.
    ///
    /// The default keeps compatibility with third-party backends. Native
    /// windowed backends override this to report each completed window while
    /// retaining the file-level SQLite state until the whole video succeeds.
    fn embed_video_segment_details_with_progress(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        on_progress(0, 0);
        self.embed_video_segment_details_with_cancel(path, cancelled)
    }

    /// Compatibility hook used by the streaming API. Older custom backends
    /// can keep returning a complete vector, while built-in windowed backends
    /// override this to publish each result as soon as it is encoded.
    fn embed_video_segment_details_with_progress_and_callback(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
        completed_segments: &std::collections::HashSet<i64>,
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        let segments = self.embed_video_segment_details_with_progress(path, cancelled, on_progress)?;
        for segment in &segments {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            if !completed_segments.contains(&(segment.segment_no as i64)) {
                on_segment(segment.clone())?;
            }
        }
        Ok(segments)
    }

    /// Encode a video one temporal window at a time and publish each completed
    /// window immediately. Native windowed backends override this hook; the
    /// default keeps compatibility with older backends by adapting their
    /// existing all-at-once result.
    fn embed_video_segment_details_streaming(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<usize, String> {
        self.embed_video_segment_details_resumable_streaming(
            path,
            cancelled,
            &std::collections::HashSet::new(),
            on_progress,
            on_segment,
        )
    }

    /// Resume a video stream from durable segment IDs. Native backends use
    /// these IDs to skip decoding and inference before work starts; legacy
    /// backends retain compatibility and filter completed vectors here.
    /// The returned count is the total number of windows, including those
    /// already completed in an earlier indexing run.
    fn embed_video_segment_details_resumable_streaming(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        completed_segments: &std::collections::HashSet<i64>,
        on_progress: &dyn Fn(usize, usize),
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<usize, String> {
        let segments = self.embed_video_segment_details_with_progress_and_callback(
            path,
            cancelled,
            on_progress,
            completed_segments,
            on_segment,
        )?;
        Ok(segments.len())
    }
}

/// A capability error means the selected backend cannot encode the requested
/// media modality. Retrying every image one by one repeats the same failure
/// and can make a large PDF index appear stuck. Callers should skip only the
/// unsupported media lane and keep any text/page vectors already produced.
pub(crate) fn is_unsupported_media_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("unsupported")
        || lower.contains("not support")
        || lower.contains("does not support")
        || lower.contains("only supports")
        || lower.contains("missing projector")
        || lower.contains("mmproj")
        || lower.contains("vision input")
        || lower.contains("image input")
        || lower.contains("audio input")
        || lower.contains("video input")
        || lower.contains("不支持")
        || lower.contains("涓嶆敮鎸")
}

fn is_out_of_memory_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("out of memory")
        || lower.contains("out_of_memory")
        || lower.contains("cuda_error_out_of_memory")
        || lower.contains("failed to allocate")
        || lower.contains("allocation failed")
}

/// One independently searchable temporal window of a video.
#[derive(Debug, Clone)]
pub struct VideoSegmentEmbedding {
    /// Stable zero-based temporal window index within the source video.
    pub segment_no: usize,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub subtitle: String,
    pub vector: Vec<f32>,
}

/// Maximum duration accepted by the multimodal audio encoders. Keeping this
/// bounded avoids context overflows while allowing long files to be covered by
/// several independently encoded windows.
pub const AUDIO_WINDOW_SECONDS: f64 = 30.0;

/// Return `(start, duration)` pairs that cover a positive duration. A final
/// short tail is padded to the media decoder's 250 ms minimum; this overlaps
/// only the final instant and prevents dropping the end of a recording.
pub fn audio_window_ranges(duration: f64) -> Vec<(f64, f64)> {
    if !duration.is_finite() || duration <= 0.0 {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0.0;
    while start < duration {
        let remaining = duration - start;
        let window = remaining.min(AUDIO_WINDOW_SECONDS).max(0.25);
        ranges.push((start, window));
        start += remaining.min(AUDIO_WINDOW_SECONDS);
    }
    ranges
}

/// Decode a small bounded queue of windows ahead while the current window is
/// encoded.  Media remains strictly single-consumer and source ordered (we
/// never batch windows from different time ranges or files), while up to
/// three CPU decoder jobs can overlap the model's current inference.  The
/// bound is intentional: it keeps long-video frame/audio buffers from
/// consuming unbounded RAM and gives cancellation/error paths a finite set of
/// workers to drain.
pub(crate) fn encode_windows_with_decode_lookahead<T, U, D, E>(
    windows: Vec<(f64, f64)>,
    decode: D,
    mut encode: E,
) -> Result<Vec<U>, String>
where
    T: Send + 'static,
    D: Fn(f64, f64) -> Result<T, String> + Send + Sync + 'static,
    E: FnMut(T) -> Result<U, String>,
{
    const PREFETCH_DEPTH: usize = 3;
    let mut ranges = windows.into_iter();
    let Some((first_start, first_duration)) = ranges.next() else {
        return Ok(Vec::new());
    };
    let decode = std::sync::Arc::new(decode);
    let spawn = |start: f64, duration: f64| {
        let decode = std::sync::Arc::clone(&decode);
        std::thread::spawn(move || decode(start, duration))
    };

    let mut queue = std::collections::VecDeque::with_capacity(PREFETCH_DEPTH);
    queue.push_back((
        first_start,
        first_duration,
        spawn(first_start, first_duration),
    ));
    while queue.len() < PREFETCH_DEPTH {
        let Some((start, duration)) = ranges.next() else { break };
        queue.push_back((start, duration, spawn(start, duration)));
    }
    let mut encoded = Vec::new();
    while let Some((start, _, decoder)) = queue.pop_front() {
        let decoded = match decoder.join() {
            Ok(Ok(decoded)) => decoded,
            Ok(Err(error)) => {
                for (_, _, decoder) in queue {
                    let _ = decoder.join();
                }
                return Err(error);
            }
            Err(_) => {
                for (_, _, decoder) in queue {
                    let _ = decoder.join();
                }
                return Err(format!("decoder worker panicked at {start:.2}s"));
            }
        };
        while queue.len() < PREFETCH_DEPTH {
            let Some((next_start, next_duration)) = ranges.next() else { break };
            queue.push_back((
                next_start,
                next_duration,
                spawn(next_start, next_duration),
            ));
        }
        match encode(decoded) {
            Ok(vector) => encoded.push(vector),
            Err(error) => {
                for (_, _, decoder) in queue {
                    let _ = decoder.join();
                }
                return Err(error);
            }
        }
    }
    Ok(encoded)
}
/// IDs used for temporal video vectors in the existing file-keyed store.
/// SQLite file IDs are positive; negative IDs therefore remain backwards
/// compatible while allowing several 768-D windows to point at one file.
const VIDEO_SEGMENT_ID_SCALE: i64 = 1_000_000;

pub fn video_segment_id(file_id: i64, segment: usize) -> i64 {
    -(file_id
        .saturating_mul(VIDEO_SEGMENT_ID_SCALE)
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

/// Reserved ID range for document page vectors. Video segments use IDs near
/// zero (negative file_id * 1_000_000); keeping pages below -4e18 makes the
/// namespaces disjoint while retaining the existing single vector store.
const DOC_PAGE_ID_BASE: i64 = -4_000_000_000_000_000_000;
const DOC_PAGE_ID_STRIDE: i64 = 1_000_000;

/// Stable IDs for one CLIP vector per document page.  Keep this namespace
/// separate from fused semantic page vectors and temporal video segments so a
/// cross-modal search can recover the exact PDF page without treating its
/// text vector as an image vector.
const DOC_IMAGE_ID_BASE: i64 = -3_000_000_000_000_000_000;
const DOC_IMAGE_ID_STRIDE: i64 = 1_000_000;

pub fn doc_page_vector_id(file_id: i64, page_no: i64) -> i64 {
    DOC_PAGE_ID_BASE
        .saturating_sub(file_id.saturating_mul(DOC_PAGE_ID_STRIDE))
        .saturating_sub(page_no.max(1))
}

pub fn is_doc_page_vector_id(id: i64) -> bool {
    id <= DOC_PAGE_ID_BASE
}

/// Decode a reserved document-page vector ID back to its source file/page.
/// Returning `None` keeps the parser conservative for unrelated negative IDs
/// used by temporal video segments.
pub fn doc_page_vector_source(id: i64) -> Option<(i64, i64)> {
    if !is_doc_page_vector_id(id) {
        return None;
    }
    let encoded = DOC_PAGE_ID_BASE.checked_sub(id)?;
    let file_id = encoded / DOC_PAGE_ID_STRIDE;
    let page_no = encoded % DOC_PAGE_ID_STRIDE;
    (file_id > 0 && page_no > 0).then_some((file_id, page_no))
}

pub fn doc_image_vector_id(file_id: i64, page_no: i64) -> i64 {
    DOC_IMAGE_ID_BASE
        .saturating_sub(file_id.saturating_mul(DOC_IMAGE_ID_STRIDE))
        .saturating_sub(page_no.max(1))
}

pub fn is_doc_image_vector_id(id: i64) -> bool {
    id <= DOC_IMAGE_ID_BASE && id > DOC_PAGE_ID_BASE
}

pub fn doc_image_vector_source(id: i64) -> Option<(i64, i64)> {
    if !is_doc_image_vector_id(id) {
        return None;
    }
    let encoded = DOC_IMAGE_ID_BASE.checked_sub(id)?;
    let file_id = encoded / DOC_IMAGE_ID_STRIDE;
    let page_no = encoded % DOC_IMAGE_ID_STRIDE;
    if file_id <= 0 || page_no <= 0 {
        return None;
    }
    Some((file_id, page_no))
}

/// 按文件类型分发到对应模态。
pub fn embed_path(e: &dyn Embedder, path: &Path, kind: &str) -> Result<Vec<f32>, String> {
    if path
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| x.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
    {
        let text = crate::textract::extract_text(path, 20_000).unwrap_or_default();
        let images = crate::textract::pdf_images(path, 8);
        return embed_pdf_parts(
            e,
            (!text.trim().is_empty()).then_some(text.as_str()),
            &images,
        );
    }
    match kind {
        "image" => e.embed_image(path),
        "audio" => e.embed_audio_full(path),
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
            Err(error) if is_unsupported_media_error(&error) => {
                // Text-only backends cannot encode any PDF raster. Do not
                // retry one image at a time; the text vector remains useful.
            }
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
    let result = embed_pdf_parts_batch_with(
        e,
        parts,
        || false,
        |index, vector| {
            output[index] = Some(vector);
        },
    );
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
    on_document: F,
) -> Result<(), String>
where
    F: FnMut(usize, Result<Vec<f32>, String>),
    C: Fn() -> bool + Sync,
{
    embed_pdf_parts_batch_with_status(e, parts, cancelled, |_, _| {}, on_document)
}

/// Like `embed_pdf_parts_batch_with`, but reports the source documents passed
/// to each actual model batch. `lane` distinguishes independent text and image
/// contexts that may encode concurrently.
pub fn embed_pdf_parts_batch_with_status<F, C, P>(
    e: &dyn Embedder,
    parts: &[(Option<String>, &[image::DynamicImage])],
    cancelled: C,
    on_model_batch: P,
    mut on_document: F,
) -> Result<(), String>
where
    F: FnMut(usize, Result<Vec<f32>, String>),
    C: Fn() -> bool + Sync,
    P: Fn(usize, &[usize]) + Sync,
{
    // The backend still applies its own token/attention budget and splits
    // oversized calls. A 16-item outer chunk gives both llama.cpp and Candle
    // enough work to keep the GPU fed, while bounding the number of decoded
    // PDF rasters retained by this routine.
    // Eight long PDF texts already fill a large CUDA context on Candle. Keep
    // the normal batch below that pressure; the recursive recovery path still
    // handles a single unusually large page without retrying the whole batch.
    const PDF_EMBED_CHUNK: usize = 8;
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
        for (chunk_index, text_chunk) in texts.chunks(PDF_EMBED_CHUNK).enumerate() {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            let source_docs = &text_map[chunk_index * PDF_EMBED_CHUNK
                ..(chunk_index * PDF_EMBED_CHUNK + text_chunk.len()).min(text_map.len())];
            on_model_batch(1, source_docs);
            match e.embed_text_batch(text_chunk) {
                Ok(rows) if rows.len() == text_chunk.len() => {
                    encoded.extend(rows.into_iter().map(Some));
                    on_model_batch(1, &[]);
                }
                _ => {
                    on_model_batch(1, &[]);
                    // A malformed/oversized batch should not serialise the
                    // entire chunk. Split it until only the offending item
                    // falls back to scalar encoding; healthy neighbours keep
                    // using native batching.
                    fn recover_text_batch(
                        e: &dyn Embedder,
                        texts: &[String],
                        source_docs: &[usize],
                        cancelled: &(impl Fn() -> bool + Sync),
                        on_model_batch: &impl Fn(usize, &[usize]),
                        encoded: &mut Vec<Option<Vec<f32>>>,
                    ) -> Result<(), String> {
                        if cancelled() {
                            return Err("index cancellation requested".into());
                        }
                        on_model_batch(1, source_docs);
                        match e.embed_text_batch(texts) {
                            Ok(rows) if rows.len() == texts.len() => {
                                encoded.extend(rows.into_iter().map(Some));
                                on_model_batch(1, &[]);
                                Ok(())
                            }
                            _ if texts.len() > 1 => {
                                on_model_batch(1, &[]);
                                let midpoint = texts.len() / 2;
                                recover_text_batch(
                                    e,
                                    &texts[..midpoint],
                                    &source_docs[..midpoint],
                                    cancelled,
                                    on_model_batch,
                                    encoded,
                                )?;
                                recover_text_batch(
                                    e,
                                    &texts[midpoint..],
                                    &source_docs[midpoint..],
                                    cancelled,
                                    on_model_batch,
                                    encoded,
                                )
                            }
                            _ => {
                                on_model_batch(1, &[]);
                                on_model_batch(1, source_docs);
                                let scalar = e.embed_text(&texts[0], false);
                                // A singleton OOM is not recoverable by
                                // retrying the same CUDA call. Mark this
                                // source absent and let the remaining batch
                                // continue, avoiding a long retry spiral.
                                encoded.push(match scalar {
                                    Err(error) if is_out_of_memory_error(&error) => None,
                                    other => other.ok(),
                                });
                                on_model_batch(1, &[]);
                                Ok(())
                            }
                        }
                    }
                    recover_text_batch(
                        e,
                        text_chunk,
                        source_docs,
                        &cancelled,
                        &on_model_batch,
                        &mut encoded,
                    )?;
                }
            }
        }
        Ok(encoded)
    };
    let encode_image_lane = || -> Result<Vec<Option<Vec<f32>>>, String> {
        let mut encoded = Vec::with_capacity(images.len());
        for (chunk_index, image_chunk) in images.chunks(PDF_EMBED_CHUNK).enumerate() {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            let source_docs = &image_map[chunk_index * PDF_EMBED_CHUNK
                ..(chunk_index * PDF_EMBED_CHUNK + image_chunk.len()).min(image_map.len())];
            on_model_batch(2, source_docs);
            match e.embed_pdf_image_data_batch(image_chunk) {
                Ok(rows) if rows.len() == image_chunk.len() => {
                    encoded.extend(rows.into_iter().map(Some));
                    on_model_batch(2, &[]);
                }
                Err(error) if is_unsupported_media_error(&error) => {
                    // A text-only backend cannot encode any image in this
                    // lane. Preserve text vectors and avoid N scalar retries.
                    encoded.extend((0..image_chunk.len()).map(|_| None));
                    on_model_batch(2, &[]);
                }
                _ => {
                    on_model_batch(2, &[]);
                    fn recover_image_batch(
                        e: &dyn Embedder,
                        images: &[&image::DynamicImage],
                        source_docs: &[usize],
                        cancelled: &(impl Fn() -> bool + Sync),
                        on_model_batch: &impl Fn(usize, &[usize]),
                        encoded: &mut Vec<Option<Vec<f32>>>,
                    ) -> Result<(), String> {
                        if cancelled() {
                            return Err("index cancellation requested".into());
                        }
                        on_model_batch(2, source_docs);
                        match e.embed_pdf_image_data_batch(images) {
                            Ok(rows) if rows.len() == images.len() => {
                                encoded.extend(rows.into_iter().map(Some));
                                on_model_batch(2, &[]);
                                Ok(())
                            }
                            Err(error) if is_unsupported_media_error(&error) => {
                                // Capability failure applies to the entire
                                // image lane, so mark all slots absent.
                                encoded.extend((0..images.len()).map(|_| None));
                                on_model_batch(2, &[]);
                                Ok(())
                            }
                            _ if images.len() > 1 => {
                                on_model_batch(2, &[]);
                                let midpoint = images.len() / 2;
                                recover_image_batch(
                                    e,
                                    &images[..midpoint],
                                    &source_docs[..midpoint],
                                    cancelled,
                                    on_model_batch,
                                    encoded,
                                )?;
                                recover_image_batch(
                                    e,
                                    &images[midpoint..],
                                    &source_docs[midpoint..],
                                    cancelled,
                                    on_model_batch,
                                    encoded,
                                )
                            }
                            _ => {
                                on_model_batch(2, &[]);
                                on_model_batch(2, source_docs);
                                let scalar = e.embed_pdf_image_data(images[0]);
                                encoded.push(match scalar {
                                    Err(error) if is_out_of_memory_error(&error) => None,
                                    other => other.ok(),
                                });
                                on_model_batch(2, &[]);
                                Ok(())
                            }
                        }
                    }
                    recover_image_batch(
                        e,
                        image_chunk,
                        source_docs,
                        &cancelled,
                        &on_model_batch,
                        &mut encoded,
                    )?;
                }
            }
        }
        Ok(encoded)
    };

    // CPU extraction and image preparation happen upstream in parallel. Model
    // inference itself is intentionally single-consumer: llama.cpp contexts
    // and CUDA execution are not generally safe to call concurrently through
    // one `Embedder`, and competing text/image lanes create GPU bubbles rather
    // than useful work. Keep batch boundaries and cancellation checks, but
    // dispatch the two lanes in deterministic order through this consumer.
    let encoded_texts = encode_text_lane()?;
    let encoded_images = encode_image_lane()?;
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
        let ext = path
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or_default();
        if ext.eq_ignore_ascii_case("gguf") {
            let name = path
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
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
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .max_depth(4)
        .into_iter()
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let name = p
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_safetensors_weight = name == "model.safetensors"
            || name == "model.safetensors.index.json"
            || (name.ends_with(".safetensors") && name.contains("-of-"));
        let arbitrary_single_weight = p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
            && p.parent().is_some_and(has_single_safetensors_file);
        if (is_safetensors_weight || arbitrary_single_weight)
            && p.parent().is_some_and(safetensors_package_complete)
        {
            safe_dirs.push(p.parent().unwrap().to_path_buf());
        }
        if p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false)
            && !name.contains("mmproj")
            && !name.contains("projector")
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
    ggufs
        .into_iter()
        .next()
        .ok_or_else(|| format!("目录中没有找到完整的 safetensors 或 GGUF 嵌入模型：{raw}"))
}

fn has_single_safetensors_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|x| x.to_str())
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
        let Ok(raw) = std::fs::read_to_string(index) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return false;
        };
        let Some(weights) = value
            .get("weight_map")
            .and_then(serde_json::Value::as_object)
        else {
            return false;
        };
        return !weights.is_empty()
            && weights
                .values()
                .all(|file| file.as_str().is_some_and(|name| dir.join(name).is_file()));
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
    let requested = configured.clamp(1, 32);
    let cpu_workers = effective_parallel_threads(cfg);
    let gpu_ceiling = selected_gpu_batch_ceiling(cfg);
    requested
        .max(cpu_workers.saturating_mul(2))
        .clamp(1, gpu_ceiling)
}

/// Bound batches using a conservative share of reported accelerator memory.
/// Device memory reports can include driver reservations and shared-memory
/// adapters, so cap the budget at 75% of total and retain a 1 GiB headroom.
/// Integrated GPUs are further limited because their reported memory is RAM.
pub fn attention_score_budget(selected_gpu: &str) -> usize {
    #[cfg(feature = "llama")]
    {
        let devices = llama::gpu_devices();
        let selected =
            if selected_gpu.trim().is_empty() || selected_gpu.eq_ignore_ascii_case("auto") {
                devices
                    .iter()
                    .filter(|device| !device.integrated)
                    .max_by_key(|device| device.free_memory)
                    .or_else(|| devices.iter().max_by_key(|device| device.free_memory))
            } else {
                devices.iter().find(|device| device.id == selected_gpu)
            };
        if let Some(device) = selected {
            let usable = usable_gpu_memory(device.total_memory, device.free_memory, device.integrated);
            return attention_score_budget_for_usable_vram(usable);
        }
    }
    1usize << 28
}

fn usable_gpu_memory(total_bytes: u64, free_bytes: u64, integrated: bool) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let total_budget = if total_bytes == 0 {
        free_bytes
    } else if integrated {
        (total_bytes / 2).min(4 * GIB)
    } else {
        (total_bytes.saturating_mul(3) / 4).min(free_bytes)
    };
    total_budget.saturating_sub(GIB)
}

fn attention_score_budget_for_usable_vram(usable_bytes: u64) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    if usable_bytes >= 8 * GIB {
        1usize << 31
    } else if usable_bytes >= 4 * GIB {
        1usize << 30
    } else if usable_bytes >= 2 * GIB {
        1usize << 29
    } else {
        1usize << 28
    }
}

fn batch_ceiling_for_usable_vram(usable_bytes: u64, integrated: bool) -> usize {
    const GIB: u64 = 1024 * 1024 * 1024;
    if integrated {
        if usable_bytes >= 3 * GIB { 8 } else { 4 }
    } else if usable_bytes >= 10 * GIB {
        32
    } else if usable_bytes >= 6 * GIB {
        16
    } else if usable_bytes >= 3 * GIB {
        8
    } else {
        4
    }
}

fn selected_gpu_batch_ceiling(cfg: &crate::config::ModelConfig) -> usize {
    #[cfg(feature = "llama")]
    {
        let devices = llama::gpu_devices();
        let selected =
            if cfg.gpu_device.trim().is_empty() || cfg.gpu_device.eq_ignore_ascii_case("auto") {
                devices
                    .iter()
                    .filter(|device| !device.integrated)
                    .max_by_key(|device| device.free_memory)
                    .or_else(|| devices.iter().max_by_key(|device| device.free_memory))
            } else {
                devices.iter().find(|device| device.id == cfg.gpu_device)
            };
        if let Some(device) = selected {
            let usable = usable_gpu_memory(device.total_memory, device.free_memory, device.integrated);
            return batch_ceiling_for_usable_vram(usable, device.integrated);
        }
    }
    // No reliable accelerator memory report: keep batches small. The
    // model-specific attention/token limits still apply inside each backend.
    8
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
        _ => ((cores as f64 * 0.90).floor() as usize)
            .min(available)
            .max(1),
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn long_audio_is_split_into_bounded_complete_windows() {
        assert!(audio_window_ranges(0.0).is_empty());
        assert!(audio_window_ranges(f64::NAN).is_empty());
        assert_eq!(audio_window_ranges(30.0), vec![(0.0, 30.0)]);
        assert_eq!(
            audio_window_ranges(61.25),
            vec![(0.0, 30.0), (30.0, 30.0), (60.0, 1.25)]
        );
    }

    #[test]
    fn decode_lookahead_keeps_order_and_joins_prefetch_on_encode_error() {
        let ordered = encode_windows_with_decode_lookahead(
            vec![(0.0, 30.0), (30.0, 30.0), (60.0, 1.0)],
            |start, _| Ok(start as usize),
            |value| Ok(value * 2),
        )
        .unwrap();
        assert_eq!(ordered, vec![0, 60, 120]);

        let future_joined = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let decoder_flag = std::sync::Arc::clone(&future_joined);
        let result = encode_windows_with_decode_lookahead(
            vec![(0.0, 30.0), (30.0, 30.0)],
            move |start, _| {
                if start > 0.0 {
                    decoder_flag.store(true, std::sync::atomic::Ordering::Release);
                }
                Ok(start as usize)
            },
            |_| Err::<(), _>("encode failed".into()),
        );
        assert!(result.is_err());
        assert!(future_joined.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn document_page_ids_round_trip_without_colliding_with_video_segments() {
        let page_id = doc_page_vector_id(42, 7);
        assert_eq!(doc_page_vector_source(page_id), Some((42, 7)));
        assert!(is_doc_page_vector_id(page_id));
        let video_id = video_segment_id(42, 7);
        assert_eq!(video_segment_source(video_id), Some(42));
        assert_eq!(doc_page_vector_source(video_id), None);
    }

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

        fn dim(&self) -> usize {
            1
        }

        fn embed_text(&self, text: &str, _query: bool) -> Result<Vec<f32>, String> {
            Ok(vec![
                text.parse::<f32>().map_err(|e| e.to_string())? + self.offset,
            ])
        }

        fn embed_image(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }
        fn embed_audio(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }
        fn embed_video(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }
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

        fn dim(&self) -> usize {
            1
        }

        fn embed_text(&self, _text: &str, _query: bool) -> Result<Vec<f32>, String> {
            self.work();
            Ok(vec![1.0])
        }

        fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.work();
            Ok(vec![vec![1.0]; texts.len()])
        }

        fn embed_image(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }

        fn embed_pdf_image_data_batch(
            &self,
            images: &[&image::DynamicImage],
        ) -> Result<Vec<Vec<f32>>, String> {
            self.work();
            Ok(vec![vec![3.0]; images.len()])
        }

        fn embed_audio(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }
        fn embed_video(&self, _path: &Path) -> Result<Vec<f32>, String> {
            Err("unused".into())
        }
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
    fn pdf_text_and_image_batches_are_serialized_and_fused() {
        let embedder = DualLaneEmbedder {
            active: std::sync::atomic::AtomicUsize::new(0),
            overlapped: std::sync::atomic::AtomicBool::new(false),
        };
        let image = image::DynamicImage::new_rgb8(2, 2);
        let parts = [(Some("page text".to_owned()), std::slice::from_ref(&image))];

        let vectors = embed_pdf_parts_batch(&embedder, &parts);
        assert_eq!(vectors[0].as_ref().unwrap(), &vec![1.0]);
        assert!(!embedder
            .overlapped
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn gpu_batch_ceiling_scales_with_usable_vram() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(batch_ceiling_for_usable_vram(12 * GIB, false), 32);
        assert_eq!(batch_ceiling_for_usable_vram(6 * GIB, false), 16);
        assert_eq!(batch_ceiling_for_usable_vram(3 * GIB, false), 8);
        assert_eq!(batch_ceiling_for_usable_vram(GIB, false), 4);
        assert_eq!(batch_ceiling_for_usable_vram(4 * GIB, true), 8);
        assert_eq!(batch_ceiling_for_usable_vram(2 * GIB, true), 4);
    }

    #[test]
    fn attention_batch_budget_tracks_free_gpu_memory() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(usable_gpu_memory(16 * GIB, 16 * GIB, false), 11 * GIB);
        assert_eq!(attention_score_budget_for_usable_vram(11 * GIB), 1usize << 31);
        assert_eq!(batch_ceiling_for_usable_vram(11 * GIB, false), 32);
        assert_eq!(usable_gpu_memory(8 * GIB, 8 * GIB, false), 5 * GIB);
        assert_eq!(batch_ceiling_for_usable_vram(5 * GIB, false), 8);
        assert_eq!(usable_gpu_memory(8 * GIB, 8 * GIB, true), 3 * GIB);
        assert_eq!(batch_ceiling_for_usable_vram(3 * GIB, true), 8);
        assert_eq!(usable_gpu_memory(16 * GIB, GIB / 2, false), 0);
        assert_eq!(batch_ceiling_for_usable_vram(0, false), 4);
    }

    #[test]
    fn model_directory_resolution_prefers_complete_safetensors_package() {
        let dir =
            std::env::temp_dir().join(format!("deepaisee-sharded-model-{}", std::process::id()));
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
        let dir =
            std::env::temp_dir().join(format!("deepaisee-custom-model-{}", std::process::id()));
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
    } else if model_path
        .extension()
        .map(|x| x.eq_ignore_ascii_case("safetensors"))
        .unwrap_or(false)
    {
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
            )
            .map(|e| Box::new(e) as Box<dyn Embedder>);
        }
        #[cfg(not(feature = "candle"))]
        return Err("此 safetensors 模型需要使用 --features candle 构建原生 Candle 后端".into());
    }
    #[cfg(feature = "llama")]
    {
        if model_path
            .extension()
            .map(|x| x.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false)
        {
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
