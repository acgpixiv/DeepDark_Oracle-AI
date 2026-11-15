//! Native Candle adapter for EmbeddingGemma2.
//!
//! The adapter is deliberately kept behind the `candle` feature. The stable
//! search/indexing core only sees `Embedder`, so another Candle model can be
//! added without changing search fusion or file scanning.

use super::{
    audio_window_ranges, encode_windows_with_decode_lookahead, EmbedInfo, Embedder,
    VideoSegmentEmbedding,
};
use embeddinggemma2::{EmbeddingGemma2, Input, Modalities, Options};
use rayon::prelude::*;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// Keep a small bounded decode queue in front of the single model consumer.
// Two windows are enough to overlap FFmpeg/image decode with model inference
// without retaining a long video's frames and PCM in memory.
// Keep enough decoded windows ahead of the single multimodal consumer to
// cover FFmpeg seek/decode jitter. Four bounded jobs hide the usual disk and
// codec gaps without creating an unbounded process storm or materially
// increasing memory pressure (each window is released after encoding).
const VIDEO_DECODE_PREFETCH_WINDOWS: usize = 4;

#[cfg(all(feature = "candle-cuda", not(feature = "llama")))]
fn cuda_ordinal_fallback(selected_device: &str) -> Result<usize, String> {
    let selected = selected_device.trim();
    if selected.is_empty() || selected.eq_ignore_ascii_case("auto") {
        return Ok(0);
    }

    // llama.cpp exposes accelerator ids as `<backend>|<device-id>`. CUDA
    // device ids are ordinals, so this also accepts the common `cuda:N`
    // spelling used by config files from older builds.
    let lower = selected.to_ascii_lowercase();
    let ordinal = if let Some(rest) = lower.strip_prefix("cuda:") {
        rest.parse::<usize>().ok()
    } else if let Some((backend, rest)) = lower.split_once('|') {
        if backend.contains("cuda") {
            rest.parse::<usize>().ok()
        } else {
            None
        }
    } else if lower.starts_with("cuda") {
        lower
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .collect::<String>()
            .parse::<usize>()
            .ok()
    } else {
        None
    };
    ordinal.ok_or_else(|| {
        format!(
            "所选设备不是可由 Candle 绑定的 CUDA 设备：{selected}；请在 GPU 列表中选择 NVIDIA CUDA 设备"
        )
    })
}

pub struct CandleEmbedder {
    model: Mutex<EmbeddingGemma2>,
    model_dir: PathBuf,
    device: String,
    vision_enabled: bool,
    audio_enabled: bool,
    video_index_audio: bool,
    audio_seconds: f32,
    context_tokens: usize,
    video_token_budget: usize,
    image_max_side: u32,
    pdf_image_token_budget: usize,
}

impl CandleEmbedder {
    pub fn load(
        dir: &Path,
        selected_device: &str,
        batch_size: usize,
        max_seq_len: usize,
        image_token_budget: usize,
        video_token_budget: usize,
        vision_enabled: bool,
        audio_enabled: bool,
        video_index_audio: bool,
        audio_seconds: f32,
        _video_frame_interval: f32,
        _video_max_frames: u32,
        image_max_side: u32,
        pdf_image_token_budget: usize,
    ) -> Result<Self, String> {
        let package_complete = crate::ai::safetensors_package_complete(dir);
        // The in-tree tokenizer runtime consumes the fast `tokenizer.json`
        // graph. A SentencePiece `tokenizer.model` can still be retained as
        // package metadata, but it is not sufficient for this native adapter.
        let has_tokenizer = dir.join("tokenizer.json").is_file();
        if !package_complete || !has_tokenizer {
            return Err(format!(
                "EmbeddingGemma2 目录缺少 config、tokenizer 或 safetensors 权重文件：{}",
                dir.display()
            ));
        }
        let mut options = Options::default();
        options.modalities = Modalities {
            vision: vision_enabled,
            audio: audio_enabled,
        };
        options.max_seq_len = max_seq_len.clamp(512, 8192);
        options.image_token_budget = image_token_budget.clamp(70, 1120);
        options.video_token_budget = video_token_budget.clamp(70, 1120);
        // The model internally splits batches by sequence length and attention
        // cost, so a larger ceiling lets short documents keep the GPU busy
        // without forcing long documents into one oversized allocation.
        options.batch_size = batch_size.clamp(1, 128);
        // The Candle attention kernels chunk query rows internally. Use the
        // selected GPU's free memory to decide how many independent vision
        // sequences may share one forward pass; busy cards stay conservative.
        options =
            options.with_attention_score_budget(crate::ai::attention_score_budget(selected_device));
        #[cfg(feature = "candle-cuda")]
        {
            let ordinal = {
                #[cfg(feature = "llama")]
                {
                    // The UI persists the stable GGML device id, while
                    // Candle expects a CUDA ordinal. Resolve it from the
                    // backend enumeration so VRAM sorting cannot change it.
                    crate::ai::llama::cuda_device_ordinal(selected_device)?
                }
                #[cfg(not(feature = "llama"))]
                {
                    cuda_ordinal_fallback(selected_device)?
                }
            };
            options = options
                .with_cuda_device(ordinal)
                .map_err(|e| format!("Candle 选择 CUDA 设备 {ordinal} 失败：{e}"))?;
            options = options.with_bf16();
        }
        if !cfg!(feature = "candle-cuda") {
            let _ = selected_device;
            return Err(
                "当前程序未集成 Candle CUDA 后端；请使用包含 candle-cuda 的构建版本".into(),
            );
        }
        let device = format!("{:?}", options.device);
        if device.to_ascii_lowercase().contains("cpu") {
            return Err("EmbeddingGemma2 Candle 后端需要 CUDA；当前机器没有可用 Candle GPU".into());
        }
        if !device.to_ascii_lowercase().contains("cuda") {
            return Err(format!("所选设备与 Candle 后端不兼容：{selected_device}"));
        }
        let model = EmbeddingGemma2::from_dir(dir, options)
            .map_err(|e| format!("Candle 加载 EmbeddingGemma2 失败：{e}"))?;
        Ok(Self {
            model: Mutex::new(model),
            model_dir: dir.to_path_buf(),
            device,
            vision_enabled,
            audio_enabled,
            video_index_audio,
            audio_seconds: audio_seconds.clamp(0.25, 30.0),
            context_tokens: max_seq_len.clamp(512, 8192),
            video_token_budget: video_token_budget.clamp(70, 1120),
            image_max_side: image_max_side.clamp(64, 4096),
            pdf_image_token_budget: pdf_image_token_budget.clamp(70, 1120),
        })
    }

    fn one(&self, input: Input, prompt: Option<&str>) -> Result<Vec<f32>, String> {
        let model = self.model.lock().map_err(|e| e.to_string())?;
        let tensor = model
            .encode(&[input], prompt)
            .map_err(|e| format!("Candle 编码失败：{e}"))?;
        let rows = tensor
            .to_vec2::<f32>()
            .map_err(|e| format!("读取 Candle 向量失败：{e}"))?;
        rows.into_iter()
            .next()
            .ok_or_else(|| "Candle 没有返回向量".into())
    }
    /// Encode a raster extracted from a PDF with the cheaper 280-token budget.
    /// Standalone photos use the model's high-resolution image budget.
    pub fn encode_pdf_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        self.one(
            Input::image_with_budget(image.clone(), self.pdf_image_token_budget),
            None,
        )
    }

    pub fn encode_pdf_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<Input> = images
            .iter()
            .map(|image| Input::image_with_budget((*image).clone(), self.pdf_image_token_budget))
            .collect();
        let model = self.model.lock().map_err(|error| error.to_string())?;
        model
            .encode(&inputs, None)
            .map_err(|error| format!("Candle PDF image batch encoding failed: {error}"))?
            .to_vec2::<f32>()
            .map_err(|error| format!("Candle PDF image vectors failed: {error}"))
    }

    // Keep usable sub-batches when a backend rejects a large vision batch.
    // This avoids turning an entire directory into scalar inference after one
    // transient allocation or model-specific batch limit is hit.
    fn encode_image_batch_adaptive(
        &self,
        images: &[&image::DynamicImage],
        pdf_budget: Option<usize>,
    ) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<Input> = images
            .iter()
            .map(|image| match pdf_budget {
                Some(budget) => Input::image_with_budget((*image).clone(), budget),
                None => Input::image((*image).clone()),
            })
            .collect();
        let encoded = {
            let model = self.model.lock().map_err(|error| error.to_string())?;
            model.encode(&inputs, None)
        };
        match encoded {
            Ok(tensor) => tensor
                .to_vec2::<f32>()
                .map_err(|error| format!("Candle image vectors failed: {error}")),
            Err(error)
                if crate::ai::is_unsupported_media_error(&error.to_string()) =>
            {
                // A missing/disabled vision tower is a capability result, not
                // an oversized batch. Return immediately so the caller can
                // preserve text vectors without retrying every image.
                Err(format!("Candle image batch encoding unsupported: {error}"))
            }
            Err(_) if images.len() > 1 => {
                let middle = images.len() / 2;
                let mut first = self.encode_image_batch_adaptive(&images[..middle], pdf_budget)?;
                first.extend(self.encode_image_batch_adaptive(&images[middle..], pdf_budget)?);
                Ok(first)
            }
            Err(error) => Err(format!("Candle image batch encoding failed: {error}")),
        }
    }

    fn video_window_details_with_cancel(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        self.video_window_details_with_cancel_and_progress(path, cancelled, &|_, _| {})
    }

    fn video_window_details_with_cancel_and_progress(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        self.video_window_details_with_cancel_and_progress_and_callback(
            path,
            cancelled,
            on_progress,
            &std::collections::HashSet::new(),
            &|_| Ok(()),
        )
    }

    fn video_window_details_with_cancel_and_progress_and_callback(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
        completed_segments: &std::collections::HashSet<i64>,
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        let duration = crate::media::require_video_duration(path)?;
        let window_seconds = crate::media::safe_video_window_seconds(
            self.context_tokens,
            self.video_token_budget,
            self.audio_enabled && self.video_index_audio,
            self.audio_seconds,
        );
        let subtitles = crate::media::video_subtitles(path);
        let include_audio = self.audio_enabled && self.video_index_audio;
        let ranges = video_window_ranges(duration, window_seconds);
        let total_windows = ranges.len();
        let pending_windows = ranges
            .into_iter()
            .enumerate()
            .filter(|(segment_no, _)| !completed_segments.contains(&(*segment_no as i64)))
            .collect::<Vec<_>>();
        let mut processed_windows = total_windows.saturating_sub(pending_windows.len());
        on_progress(processed_windows, total_windows);
        std::thread::scope(|scope| {
            let mut segments = Vec::new();
            let spawn_decode = |start: f64, end: f64| {
                let path = path.to_path_buf();
                let max_side = self.image_max_side;
                let include_video = self.vision_enabled;
                let should_cancel = || cancelled();
                scope.spawn(move || {
                    let (frames, audio) = crate::media::decode_video_window_with_cancel(
                        &path,
                        start,
                        end - start,
                        max_side,
                        include_video,
                        include_audio,
                        16_000,
                        &should_cancel,
                    );
                    if should_cancel() {
                        for frame in &frames {
                            let _ = std::fs::remove_file(frame);
                        }
                        if let Some(parent) = frames.first().and_then(|frame| frame.parent()) {
                            let _ = std::fs::remove_dir_all(parent);
                        }
                        return (Vec::new(), None);
                    }
                    let images = frames
                        .iter()
                        .filter_map(|frame| {
                            let image = image::open(frame).ok();
                            let _ = std::fs::remove_file(frame);
                            image
                        })
                        .collect::<Vec<_>>();
                    if let Some(parent) = frames.first().and_then(|frame| frame.parent()) {
                        let _ = std::fs::remove_dir_all(parent);
                    }
                    (images, audio)
                })
            };
            let mut windows = pending_windows.into_iter();
            let mut decode_queue = VecDeque::with_capacity(VIDEO_DECODE_PREFETCH_WINDOWS);
            for _ in 0..VIDEO_DECODE_PREFETCH_WINDOWS {
                let Some((segment_no, (start, end))) = windows.next() else { break };
                decode_queue.push_back((segment_no, start, end, spawn_decode(start, end)));
            }

            while let Some((segment_no, start, end, decode_job)) = decode_queue.pop_front() {
                let (images, audio) = decode_job
                    .join()
                    .map_err(|_| "video window decoder thread panicked".to_string())?;
            if cancelled() {
                for (_, _, _, job) in decode_queue {
                    let _ = job.join();
                }
                return Err("index cancellation requested".into());
            }
            // Refill the bounded queue before model inference. All queued
            // decoders run while the current window is encoded, so CPU/FFmpeg
            // work stays ahead without changing the sequential media model
            // policy.
            if let Some((next_segment_no, (next_start, next_end))) = windows.next() {
                decode_queue.push_back((
                    next_segment_no,
                    next_start,
                    next_end,
                    spawn_decode(next_start, next_end),
                ));
            }
            let subtitle = crate::media::subtitle_text_for_window_with_timestamps(
                &subtitles,
                start,
                end,
            );
            let encoded = (|| {
                let mut vector = if !images.is_empty()
                    || audio.as_ref().is_some_and(|samples| !samples.is_empty())
                {
                    let mut input = Input::new(Vec::new());
                    if !images.is_empty() {
                        input = input.with_video(images);
                    }
                    if let Some(samples) = audio {
                        input = input.with_audio(samples);
                    }
                    Some(self.one(input, None)?)
                } else {
                    None
                };

                // Subtitles are persisted verbatim and searched by the
                // timestamp-aware SQLite FTS lane. Avoid a second text-model
                // forward for every media window; it doubled video indexing
                // work and diluted the native visual/audio signal. Keep a
                // text fallback only when a window has no decodable media.
                if vector.is_none() && !subtitle.is_empty() {
                    let text = format!(
                        "title: {} | text: {}",
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("video"),
                        subtitle
                    );
                    vector = Some(self.embed_text(&text, false)?);
                }

                Ok::<_, String>(vector)
            })();
            let vector = match encoded {
                Ok(vector) => vector,
                Err(error) => {
                    if error.contains("cancellation") {
                        for (_, _, _, next_job) in decode_queue {
                            let _ = next_job.join();
                        }
                        return Err(error);
                    }
                    // A malformed frame/audio window must not discard every
                    // other window in the same video. Count this window as
                    // processed and continue with the queued decoders.
                    processed_windows = processed_windows.saturating_add(1);
                    on_progress(processed_windows, total_windows);
                    continue;
                }
            };

            if cancelled() {
                for (_, _, _, next_job) in decode_queue {
                    let _ = next_job.join();
                }
                return Err("index cancellation requested".into());
            }

            if let Some(vector) = vector {
                let segment = VideoSegmentEmbedding {
                    segment_no,
                    start_seconds: start,
                    end_seconds: end,
                    subtitle,
                    vector,
                };
                on_segment(segment.clone())?;
                segments.push(segment);
            }
            // Progress is based on windows decoded, not vectors emitted. A
            // silent/blank window is still complete work and should advance
            // the indicator instead of making it appear stalled.
            processed_windows = processed_windows.saturating_add(1);
            on_progress(processed_windows, total_windows);
        }

        if segments.is_empty() && processed_windows < total_windows {
            return Err("视频未能解码出画面、音轨或字幕窗口（需要 FFmpeg）".into());
        }
        Ok(segments)
        })
    }
}

fn video_window_ranges(duration: f64, window_seconds: f64) -> Vec<(f64, f64)> {
    let mut ranges = Vec::new();
    let mut start = 0.0;
    while start < duration {
        let end = (start + window_seconds).min(duration);
        if end <= start {
            break;
        }
        ranges.push((start, end));
        start = end;
    }
    ranges
}

impl Embedder for CandleEmbedder {
    fn info(&self) -> EmbedInfo {
        EmbedInfo {
            backend: "candle".into(),
            model: self.model_dir.display().to_string(),
            dim: 768,
            device: self.device.clone(),
            modalities: {
                let mut values = vec!["text".into()];
                if self.vision_enabled {
                    values.extend(["image".into(), "video".into()]);
                }
                if self.audio_enabled {
                    values.push("audio".into());
                }
                values
            },
            ready: true,
            detail: "EmbeddingGemma2 Candle 原生多模态后端 · 文本/图片/音频/视频共用 768 维空间"
                .into(),
        }
    }

    fn dim(&self) -> usize {
        768
    }

    fn embed_text(&self, text: &str, query: bool) -> Result<Vec<f32>, String> {
        if !query {
            return self
                .embed_text_batch(&[text.to_owned()])?
                .into_iter()
                .next()
                .ok_or_else(|| "Candle 没有返回文本向量".into());
        }
        let model = self.model.lock().map_err(|e| e.to_string())?;
        let tensor = model
            .encode_queries(&[text])
            .map_err(|e| format!("Candle 文本编码失败：{e}"))?;
        tensor
            .to_vec2::<f32>()
            .map_err(|e| format!("读取 Candle 文本向量失败：{e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| "Candle 没有返回文本向量".into())
    }

    fn embed_text_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Leave room for the model's task prefix under its 8192-token limit.
        const MAX_CHUNK_TOKENS: usize = 8_000;
        const MAX_CHUNK_CHARS: usize = 3_500;
        const OVERLAP_CHARS: usize = 128;
        let model = self.model.lock().map_err(|e| e.to_string())?;
        let tokenizer = model.tokenizer();
        let mut chunks = Vec::new();
        let mut owners = Vec::new();
        for (owner, text) in texts.iter().enumerate() {
            let chars: Vec<char> = text.chars().collect();
            if chars.is_empty() {
                chunks.push(String::new());
                owners.push(owner);
                continue;
            }
            let mut start = 0usize;
            while start < chars.len() {
                let mut end = (start + MAX_CHUNK_CHARS).min(chars.len());
                loop {
                    let chunk: String = chars[start..end].iter().collect();
                    let token_count = tokenizer
                        .encode(chunk.as_str(), true)
                        .map_err(|error| format!("Candle 文本分块计数失败：{error}"))?
                        .get_ids()
                        .len();
                    if token_count <= MAX_CHUNK_TOKENS {
                        chunks.push(chunk);
                        owners.push(owner);
                        break;
                    }
                    let span = end - start;
                    if span <= 1 {
                        return Err("Candle 文本存在超过模型上下文长度的单字符片段".into());
                    }
                    end = start + (span / 2).max(1);
                }
                if end == chars.len() {
                    break;
                }
                start = end.saturating_sub(OVERLAP_CHARS.min(end - start - 1));
            }
        }
        let rows = model
            .encode_documents(&chunks)
            .map_err(|e| format!("Candle 批量文本编码失败：{e}"))?
            .to_vec2::<f32>()
            .map_err(|e| format!("读取 Candle 批量向量失败：{e}"))?;
        let mut sums = vec![Vec::<f32>::new(); texts.len()];
        let mut counts = vec![0usize; texts.len()];
        for (row, &owner) in rows.into_iter().zip(&owners) {
            if sums[owner].is_empty() {
                sums[owner] = vec![0.0; row.len()];
            }
            if row.len() != sums[owner].len() {
                continue;
            }
            for (dst, value) in sums[owner].iter_mut().zip(row) {
                *dst += value;
            }
            counts[owner] += 1;
        }
        if counts.iter().any(|&count| count == 0) {
            return Err("Candle 批量编码没有返回完整的文档向量".into());
        }
        for (sum, &count) in sums.iter_mut().zip(&counts) {
            for value in sum.iter_mut() {
                *value /= count as f32;
            }
            crate::ai::l2_normalize(sum);
        }
        Ok(sums)
    }

    fn embed_image(&self, path: &Path) -> Result<Vec<f32>, String> {
        let image = image::open(path).map_err(|e| format!("图片读取失败：{e}"))?;
        self.one(Input::image(image), None)
    }

    fn embed_image_batch(&self, paths: &[&Path]) -> Result<Vec<Vec<f32>>, String> {
        let inputs: Vec<Input> = paths
            .par_iter()
            .map(|path| {
                image::open(path)
                    .map(Input::image)
                    .map_err(|error| format!("读取图片 {} 失败：{error}", path.display()))
            })
            .collect::<Result<_, _>>()?;
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.model.lock().map_err(|error| error.to_string())?;
        model
            .encode(&inputs, None)
            .map_err(|error| format!("Candle 批量图片编码失败：{error}"))?
            .to_vec2::<f32>()
            .map_err(|error| format!("读取 Candle 图片向量失败：{error}"))
    }

    fn embed_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        self.one(Input::image(image.clone()), None)
    }

    fn embed_pdf_image_data(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        CandleEmbedder::encode_pdf_image_data(self, image)
    }

    fn embed_pdf_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.encode_image_batch_adaptive(images, Some(self.pdf_image_token_budget))
    }

    fn embed_video_frame_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.encode_image_batch_adaptive(images, Some(self.video_token_budget))
    }

    fn embed_image_data_batch(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.encode_image_batch_adaptive(images, None)
    }

    fn embed_audio(&self, path: &Path) -> Result<Vec<f32>, String> {
        let vectors = self.embed_audio_segments(path)?;
        let mut merged = vec![0.0f32; self.dim()];
        let mut count = 0usize;
        for vector in vectors {
            if vector.len() != merged.len() {
                continue;
            }
            for (dst, value) in merged.iter_mut().zip(vector) {
                *dst += value;
            }
            count += 1;
        }
        if count == 0 {
            return Err("音频没有可编码的有效分段".into());
        }
        for value in &mut merged {
            *value /= count as f32;
        }
        crate::ai::l2_normalize(&mut merged);
        Ok(merged)
    }

    fn embed_audio_segments(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        let duration = crate::media::media_duration(path)
            .ok_or_else(|| format!("无法读取音频时长：{}", path.display()))?;
        let decode_path = path.to_path_buf();
        let path_display = path.display().to_string();
        let vectors = encode_windows_with_decode_lookahead(
            audio_window_ranges(duration),
            move |start, seconds| {
                crate::media::load_audio_segment_at_rate(
                    &decode_path,
                    start,
                    seconds,
                    16_000,
                )
                .ok_or_else(|| {
                    format!("音频分段解码失败：{} ({start:.2}s)", path_display)
                })
            },
            |samples| self.one(Input::audio(samples), None),
        )?;
        if vectors.is_empty() {
            return Err("音频没有可编码的分段".into());
        }
        Ok(vectors)
    }

    fn video_window_vectors(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        // Process one context-safe window at a time. Each window mixes 1 FPS
        // frames and its matching 16 kHz audio clip in EmbeddingGemma2's
        // shared context. Smaller-context model configurations therefore get
        // more, shorter vectors instead of one failed oversized request.
        let duration = crate::media::require_video_duration(path)?;
        let window_seconds = crate::media::safe_video_window_seconds(
            self.context_tokens,
            self.video_token_budget,
            self.audio_enabled && self.video_index_audio,
            self.audio_seconds,
        );
        let mut vectors = Vec::new();
        let mut start = 0.0f64;
        while start < duration {
            let window = (duration - start).min(window_seconds);
            let include_audio = self.audio_enabled && self.video_index_audio;
            let (frames, audio) = crate::media::decode_video_window(
                path,
                start,
                window,
                self.image_max_side,
                self.vision_enabled,
                include_audio,
                16_000,
            );
            let images: Vec<image::DynamicImage> = frames
                .iter()
                .filter_map(|frame| {
                    let image = image::open(frame).ok();
                    let _ = std::fs::remove_file(frame);
                    image
                })
                .collect();
            if let Some(parent) = frames.first().and_then(|frame| frame.parent()) {
                let _ = std::fs::remove_dir_all(parent);
            }
            if !images.is_empty() || audio.as_ref().is_some_and(|samples| !samples.is_empty()) {
                let mut input = Input::new(Vec::new());
                if !images.is_empty() {
                    input = input.with_video(images);
                }
                if let Some(samples) = audio {
                    input = input.with_audio(samples);
                }
                vectors.push(self.one(input, None)?);
            }
            start += window;
        }
        if vectors.is_empty() {
            return Err("视频未能解码出画面或音轨（需要 ffmpeg）".into());
        }
        Ok(vectors)
    }

    fn embed_video(&self, path: &Path) -> Result<Vec<f32>, String> {
        let vectors = self.video_window_vectors(path)?;
        let mut merged = vec![0.0f32; vectors[0].len()];
        let mut count = 0usize;
        for vector in vectors {
            if vector.len() != merged.len() {
                continue;
            }
            for (dst, value) in merged.iter_mut().zip(vector) {
                *dst += value;
            }
            count += 1;
        }
        if count == 0 {
            return Err("video windows returned inconsistent vector dimensions".into());
        }
        for value in &mut merged {
            *value /= count as f32;
        }
        crate::ai::l2_normalize(&mut merged);
        Ok(merged)
    }

    fn embed_video_segments(&self, path: &Path) -> Result<Vec<Vec<f32>>, String> {
        self.video_window_vectors(path)
    }

    fn embed_video_segment_details(
        &self,
        path: &Path,
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        self.video_window_details_with_cancel(path, &|| false)
    }

    fn embed_video_segment_details_with_cancel(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        self.video_window_details_with_cancel_and_progress(path, cancelled, &|_, _| {})
    }

    fn embed_video_segment_details_with_progress(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
    ) -> Result<Vec<VideoSegmentEmbedding>, String> {
        self.video_window_details_with_cancel_and_progress(path, cancelled, on_progress)
    }

    fn embed_video_segment_details_streaming(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        on_progress: &dyn Fn(usize, usize),
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<usize, String> {
        let segments = self.video_window_details_with_cancel_and_progress_and_callback(
            path,
            cancelled,
            on_progress,
            &std::collections::HashSet::new(),
            on_segment,
        )?;
        Ok(segments.len())
    }

    fn embed_video_segment_details_resumable_streaming(
        &self,
        path: &Path,
        cancelled: &(dyn Fn() -> bool + Sync),
        completed_segments: &std::collections::HashSet<i64>,
        on_progress: &dyn Fn(usize, usize),
        on_segment: &dyn Fn(VideoSegmentEmbedding) -> Result<(), String>,
    ) -> Result<usize, String> {
        self.video_window_details_with_cancel_and_progress_and_callback(
            path,
            cancelled,
            on_progress,
            completed_segments,
            on_segment,
        )?;
        let duration = crate::media::require_video_duration(path)?;
        let window_seconds = crate::media::safe_video_window_seconds(
            self.context_tokens,
            self.video_token_budget,
            self.audio_enabled && self.video_index_audio,
            self.audio_seconds,
        );
        Ok(video_window_ranges(duration, window_seconds).len())
    }

    /*
    fn embed_video_legacy(&self, path: &Path) -> Result<Vec<f32>, String> {
        // 与 index.rs 用同一套智能抽帧：场景切换/I 帧优先 + dHash 去重，
        // 避免「固定均匀切 32 帧」在长静止镜头上浪费算力、在快剪视频上漏画面。
        let frames = crate::media::sample_video_frames_smart(
            path,
            crate::media::FramePlan {
                interval: self.video_frame_interval,
                max_frames: self.video_max_frames,
                max_side: self.image_max_side,
                dedup_distance: 6,
            },
        );
        if frames.is_empty() {
            return Err("视频抽帧失败（需要 ffmpeg）".into());
        }
        let mut images = Vec::with_capacity(frames.len());
        for frame in &frames {
            if let Ok(image) = image::open(frame) {
                images.push(image);
            }
            let _ = std::fs::remove_file(frame);
        }
        if images.is_empty() {
            return Err("视频帧读取失败".into());
        }
        self.one(Input::video(images), None)
    }
    */
}

#[cfg(test)]
mod video_window_tests {
    use super::video_window_ranges;

    #[test]
    fn video_windows_keep_exact_boundaries_for_partial_tail() {
        assert_eq!(
            video_window_ranges(65.0, 30.0),
            vec![(0.0, 30.0), (30.0, 60.0), (60.0, 65.0)]
        );
    }
}
