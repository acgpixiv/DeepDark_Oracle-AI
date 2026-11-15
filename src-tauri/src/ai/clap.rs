//! 跨模态（文字 ↔ 声音）编码器：CLAP（Contrastive Language-Audio Pretraining）。
//!
//! 和 `clip.rs` 是同一套思路，只是把「像素」换成「频谱」：音频塔吃
//! 48kHz 单声道的 log-mel 频谱，文本塔吃 BERT token，两者被训练到同一个
//! 512 维空间里，所以「一段说话声/雨声/钢琴曲」可以用一句话找出来。
//!
//! 模型来自 `Xenova/clap-htsat-unfused` 的 ONNX 导出，拆成两个文件：
//! `audio_model.onnx`（约 34MB 量化版）与 `text_model.onnx`（约 127MB 量化版）。
//! 缺文件时返回 `None`，调用方静默降级，不影响其它检索方式。

use std::path::{Path, PathBuf};

use super::bpe::Bpe;
use super::ort::{Data, Ort, Session};

/// CLAP 的固定采样率。
pub const SAMPLE_RATE: usize = 48_000;
/// 一次喂进去的音频长度（秒）：模型训练时就是 10 秒，改短会掉点。
const SECONDS: usize = 10;/// log-mel 频谱参数，与 `clap_preprocessor_config.json` 一致。
const N_FFT: usize = 1024;
const HOP: usize = 480;
const MEL_BINS: usize = 64;
const F_MIN: f32 = 50.0;
const F_MAX: f32 = 14_000.0;
/// 帧数：input_features 的形状是 [1, 1, 4, 1001, 64]，
/// 4 是 HTSAT 的「4 段拼接」维度，这里按时长取 4×256。
const FRAMES: usize = 1000 + 1;
const TEXT_MAX: usize = 64;

pub struct ClapModel {
    _ort: Ort,
    audio: std::sync::Mutex<Session>,
    text: std::sync::Mutex<Session>,
    bpe: Bpe,
    dim: usize,
    dir: std::path::PathBuf,
}

impl ClapModel {
    /// 从目录加载：`audio_model.onnx` + `text_model.onnx` + `vocab.json` + `merges.txt`。
    ///
    /// 四个文件缺一不可——只加载一边的话，向量不在同一空间，检索结果全是噪声。
    pub fn load(dir: &Path, threads: i32) -> Option<ClapModel> {
        let a = dir.join("audio_model.onnx");
        let t = dir.join("text_model.onnx");
        if !a.exists() || !t.exists() {
            return None;
        }
        let dll = super::clip::find_dll()?;
        let ort = Ort::load(&dll, threads).ok()?;
        let audio = ort.session(&a).ok()?;
        let text = ort.session(&t).ok()?;
        let bpe = Bpe::load(dir)?;
        Some(ClapModel {
            _ort: ort,
            audio: std::sync::Mutex::new(audio),
            text: std::sync::Mutex::new(text),
            bpe,
            dim: 512,
            dir: dir.to_path_buf(),
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 给「模型」页显示的一行状态。
    pub fn describe(&self) -> String {
        format!("CLAP · {} 维 · {}", self.dim, self.dir.display())
    }

    /// 文本 → 512 维向量（已归一化）。
    ///
    /// 注意中文：CLAP 的词表是英文 BPE，中文会被拆成一串字节 token，
    /// 语义基本丢失。调用方在编码前会把中文先过一遍小词表（见 `search.rs`）。
    pub fn text_vec(&self, text: &str) -> Result<Vec<f32>, String> {
        let ids = self.bpe.encode(text, TEXT_MAX);
        // Xenova 的 CLAP 文本塔导出时把 attention_mask 换成了 token_type_ids，
        // 且只保留这两路输入；喂 attention_mask 会报 "Invalid input name"。
        let ids_shape = [1i64, TEXT_MAX as i64];
        let sess = self.text.lock().map_err(|e| e.to_string())?;
        let out = sess.run(
            &[("input_ids", &ids_shape, Data::I64(&ids))],
            &["text_embeds"],
        )?;
        let mut v = out.into_iter().next().ok_or("CLAP 文本塔没有返回向量")?;
        normalize(&mut v);
        Ok(v)
    }

    /// 音频 → 512 维向量（已归一化）。
    ///
    /// `pcm` 是 16 位单声道、任意采样率的样本；内部会重采样到 48kHz
    /// 并补齐/截断到 10 秒——CLAP 的位置编码是固定的，长度不对会直接报错。
    pub fn audio_vec(&self, path: &Path) -> Result<Vec<f32>, String> {
        let Some(duration) = crate::media::media_duration(path)
            .filter(|value| value.is_finite() && *value > 0.0)
        else {
            // Some containers do not expose duration metadata. Preserve the
            // previous behaviour in that case instead of failing indexing.
            let pcm = crate::media::load_audio_48k(path, SECONDS as f32)
                .ok_or_else(|| "读不出音频（需要 ffmpeg）".to_string())?;
            return self.audio_vec_pcm(&pcm);
        };

        let mut fused = vec![0.0f32; self.dim];
        let mut total_weight = 0.0f32;
        for (start, window) in audio_windows(duration) {
            let pcm = crate::media::load_audio_segment_at_rate(
                path,
                start,
                window,
                SAMPLE_RATE as u32,
            )
            .ok_or_else(|| format!("读不出音频窗口（{start:.1}s）（需要 ffmpeg）"))?;
            let vector = self.audio_vec_pcm(&pcm)?;
            // Weight by source duration so a short tail cannot skew a long
            // recording as much as a complete 10-second window.
            let weight = window as f32;
            for (dst, src) in fused.iter_mut().zip(vector) {
                *dst += src * weight;
            }
            total_weight += weight;
        }
        if total_weight <= f32::EPSILON {
            return Err("音频没有可用的窗口输入".into());
        }
        for value in &mut fused {
            *value /= total_weight;
        }
        normalize(&mut fused);
        Ok(fused)
    }
    /// 已经解码好的 48kHz 单声道波形 → 向量（索引线程批量使用时省一次解码）。
    pub fn audio_vec_pcm(&self, pcm: &[f32]) -> Result<Vec<f32>, String> {
        let feats = log_mel(pcm);
        // 导出的 audio_model.onnx 需要 4 维：[batch, 通道, 帧数, 64]
        let shape = [1i64, 1, FRAMES as i64, MEL_BINS as i64];
        let sess = self.audio.lock().map_err(|e| e.to_string())?;
        let out = sess.run(
            &[("input_features", &shape, Data::F32(&feats))],
            &["audio_embeds"],
        )?;
        let mut v = out.into_iter().next().ok_or("CLAP 音频塔没有返回向量")?;
        normalize(&mut v);
        Ok(v)
    }

    /// 诊断用：返回中间 log-mel 特征。
    pub fn debug_mel(&self, pcm: &[f32]) -> Vec<f32> {
        log_mel(pcm)
    }

    /// 诊断用：直接喂特征张量。
    pub fn raw_audio(&self, feats: &[f32], shape: &[i64]) -> Result<Vec<f32>, String> {
        let sess = self.audio.lock().map_err(|e| e.to_string())?;
        let out = sess.run(
            &[("input_features", shape, Data::F32(feats))],
            &["audio_embeds"],
        )?;
        Ok(out.into_iter().next().unwrap_or_default())
    }
}

/// 把整段波形转成 [1001,64] 的 log-mel（dB）。
///
/// 三个细节必须和 HuggingFace 的 `ClapFeatureExtractor` 对齐，差一个检索质量就崩：
///   1. mel 刻度用 **Slaney**（不是 HTK），滤波器组也按 Slaney 做面积归一化；
///   2. 补长用 **repeatpad**（循环重复整段音频），不是补零——短提示音补零后
///      99% 是静音，特征会被拉平，什么都不像；
///   3. 输出就是 **dB**（`10*log10(max(x,1e-10))`），**不要再做 peak-8 截断或
///      任何线性缩放**——实测多做一步 `max(x,peak-8)`+`(x+4)/4` 会让不同音频的
///      余弦相似度从 0.23~0.66 全部塌到 0.995 以上，等于完全没有区分度。
fn log_mel(pcm: &[f32]) -> Vec<f32> {
    let total = FRAMES * MEL_BINS;
    let mut buf = vec![-100f32; total];
    if pcm.is_empty() {
        return buf;
    }
    let mut frame = 0usize;
    while frame < FRAMES {
        // 与 librosa 的 center=True 一致：左右各反射填充 N_FFT/2，
        // 这样第 0 帧以第一个采样点为中心。
        let start = frame * HOP;
        let mut seg = [0f32; N_FFT];
        for (i, s) in seg.iter_mut().enumerate() {
            *s = sample_reflect(pcm, start as isize + i as isize - (N_FFT / 2) as isize);
        }
        let mel = frame_mel(&seg);
        for (i, v) in mel.iter().enumerate() {
            buf[frame * MEL_BINS + i] = v.max(1e-10).log10() * 10.0;
        }
        frame += 1;
    }
    buf
}

/// 反射取样本（等价于 numpy 的 `pad(mode="reflect")`）；越界时折回来。
fn sample_reflect(pcm: &[f32], mut idx: isize) -> f32 {
    let n = pcm.len() as isize;
    if n == 1 {
        return pcm[0];
    }
    while idx < 0 || idx >= n {
        if idx < 0 {
            idx = -idx;
        }
        if idx >= n {
            idx = 2 * (n - 1) - idx;
        }
    }
    pcm[idx as usize]
}

/// 单帧：加汉宁窗 → FFT → 功率谱 → 64 个三角滤波器 → 对数。
fn frame_mel(seg: &[f32; N_FFT]) -> [f32; MEL_BINS] {
    let mut re = [0f32; N_FFT];
    let mut im = [0f32; N_FFT];
    for i in 0..N_FFT {
        // 周期性汉宁窗，和 librosa/torchaudio 的 `hann` 默认一致。
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos();
        re[i] = seg[i] * w;
    }
    fft(&mut re, &mut im);
    let mut power = [0f32; N_FFT / 2 + 1];
    for i in 0..power.len() {
        power[i] = re[i] * re[i] + im[i] * im[i];
    }
    let filters = mel_filters();
    let mut out = [0f32; MEL_BINS];
    for (m, f) in filters.iter().enumerate() {
        let mut acc = 0f32;
        for (k, w) in f.iter().enumerate() {
            acc += power[k] * w;
        }
        out[m] = acc;
    }
    out
}

/// 64 个三角滤波器的权重（**Slaney** mel 刻度，50Hz~14kHz，Slaney 面积归一化）。
///
/// 只算一次：调用方是后台索引线程，每 10 秒音频要跑 1001 帧，
/// 每次重算三角权重会白烧几毫秒。
fn mel_filters() -> &'static Vec<Vec<f32>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Vec<Vec<f32>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        // Slaney 刻度：1kHz 以下线性，以上对数
        let hz2mel = |hz: f32| {
            const MIN_LOG_HZ: f32 = 1000.0;
            const MIN_LOG_MEL: f32 = 15.0;
            let logstep = 27.0 / 6.4f32.ln();
            if hz >= MIN_LOG_HZ {
                MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() * logstep
            } else {
                3.0 * hz / 200.0
            }
        };
        let mel2hz = |m: f32| {
            const MIN_LOG_HZ: f32 = 1000.0;
            const MIN_LOG_MEL: f32 = 15.0;
            let logstep = 6.4f32.ln() / 27.0;
            if m >= MIN_LOG_MEL {
                MIN_LOG_HZ * (logstep * (m - MIN_LOG_MEL)).exp()
            } else {
                200.0 * m / 3.0
            }
        };
        let lo = hz2mel(F_MIN);
        let hi = hz2mel(F_MAX);
        let mels: Vec<f32> = (0..MEL_BINS + 2)
            .map(|i| lo + (hi - lo) * i as f32 / (MEL_BINS + 1) as f32)
            .collect();
        let mels_hz: Vec<f32> = mels.iter().map(|m| mel2hz(*m)).collect();
        // 和 HF 的 `_create_triangular_filter_bank` 一样按**频率**插值，
        // 而不是按 bin 整数下标取三角——后者会丢掉 1 个 bin 内的细节，
        // 高频段滤波器只有一个 bin 宽时差别尤其明显。
        let nbins = N_FFT / 2 + 1;
        let fft_freqs: Vec<f32> = (0..nbins)
            .map(|i| (SAMPLE_RATE / 2) as f32 * i as f32 / (nbins - 1) as f32)
            .collect();
        let mut out = Vec::with_capacity(MEL_BINS);
        for m in 0..MEL_BINS {
            let (lo_hz, ce_hz, hi_hz) = (mels_hz[m], mels_hz[m + 1], mels_hz[m + 2]);
            let mut w = vec![0f32; nbins];
            for (k, &f) in fft_freqs.iter().enumerate() {
                if f > lo_hz && f <= ce_hz {
                    w[k] = (f - lo_hz) / (ce_hz - lo_hz);
                } else if f > ce_hz && f < hi_hz {
                    w[k] = (hi_hz - f) / (hi_hz - ce_hz);
                }
            }
            // Slaney 归一化：按滤波器带宽缩放，让各频带能量可比
            let norm = 2.0 / (hi_hz - lo_hz).max(1e-6);
            for v in w.iter_mut() {
                *v *= norm;
            }
            out.push(w);
        }
        out
    })
}

/// 原地基 2 FFT（长度必须是 2 的幂，N_FFT=1024 满足）。
fn fft(re: &mut [f32; N_FFT], im: &mut [f32; N_FFT]) {
    let n = N_FFT;
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ar, ai) = (re[i + k], im[i + k]);
                let (br, bi) = (
                    re[i + k + len / 2] * cr - im[i + k + len / 2] * ci,
                    re[i + k + len / 2] * ci + im[i + k + len / 2] * cr,
                );
                re[i + k] = ar + br;
                im[i + k] = ai + bi;
                re[i + k + len / 2] = ar - br;
                im[i + k + len / 2] = ai - bi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Split a media duration into contiguous CLAP windows. The final window is
/// allowed to be shorter than ten seconds; the feature extractor pads it to
/// the fixed input shape. Keeping this policy separate makes the coverage
/// guarantee easy to test without loading ONNX models.
fn audio_windows(duration: f64) -> Vec<(f64, f64)> {
    if !duration.is_finite() || duration <= 0.0 {
        return Vec::new();
    }
    let mut windows = Vec::new();
    let mut start = 0.0;
    while start < duration {
        let length = (duration - start).min(SECONDS as f64).max(0.25);
        windows.push((start, length));
        start += SECONDS as f64;
    }
    windows
}
fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-8 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// 中文关键词 → 英文提示词。
///
/// `laion/clap-htsat-unfused` 是拿英文音频描述训练的，直接喂中文会退化成
/// 「什么都能匹配一点」：实测「钢琴」被判成吉他、「雨声」输给钢琴。把常见的
/// 中文说法映射成英文提示后，合成的 6 段音频命中率从 13/36 提到 13/15。
const PROMPTS: &[(&str, &str)] = &[
    ("钢琴", "piano music"),
    ("下雨", "rain falling"),
    ("雨声", "rain falling"),
    ("雷", "thunder"),
    ("鼓", "drums beating"),
    ("架子鼓", "a drum solo"),
    ("打鼓", "drums beating"),
    ("说话", "a person speaking"),
    ("讲话", "a person speaking"),
    ("人声", "a person speaking"),
    ("聊天", "people talking"),
    ("唱歌", "someone singing"),
    ("歌声", "someone singing"),
    ("音乐", "music"),
    ("鸟", "birds chirping"),
    ("吉他", "an electric guitar"),
    ("海浪", "ocean waves"),
    ("波浪", "ocean waves"),
    ("流水", "flowing water"),
    ("小溪", "flowing water"),
    ("水声", "flowing water"),
    ("狗", "a dog barking"),
    ("猫", "a cat meowing"),
    ("汽车", "a car horn"),
    ("喇叭", "a car horn"),
    ("警笛", "an alarm siren"),
    ("警报", "an alarm siren"),
    ("电话", "a telephone ringing"),
    ("铃声", "a telephone ringing"),
    ("风", "wind blowing"),
    ("掌声", "applause"),
    ("脚步", "footsteps"),
    ("键盘", "typing on a keyboard"),
    ("婴儿", "a baby crying"),
    ("爆炸", "an explosion"),
    ("雨", "the sound of rain"),
];

/// 把中文查询翻译成 CLAP 能听懂的英文提示。
///
/// 命中多个关键词就拼成 `a, b` 的形式（CLAP 支持多标签描述）；一个都没命中
/// 就原样返回——用户可能本来就在用英文描述。
pub fn prompt_for(text: &str) -> String {
    let mut hits: Vec<(usize, &str)> = Vec::new();
    for (cn, en) in PROMPTS {
        if text.contains(cn) && !hits.iter().any(|(_, e)| *e == *en) {
            hits.push((cn.chars().count(), en));
        }
    }
    if hits.is_empty() {
        return text.to_string();
    }
    // 长关键词优先，让「下雨」盖过「雨」、「架子鼓」盖过「鼓」。
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<&str> = Vec::new();
    for (_, en) in hits {
        if !out.contains(&en) {
            out.push(en);
        }
    }
    out.join(", ")
}

/// 这句话是不是在问「声音」。
///
/// 命中声音词表就算——「钢琴」「打鼓」「鸟叫」都是明确的找声音意图，
/// 这时不该再拿图片来凑数。注意这和 `prompt_for` 不同：英文查询本来就
/// 原样返回，但它同样算找声音（表里有英文对照）。
pub fn sound_intent(text: &str) -> bool {
    let lower = text.to_lowercase();
    PROMPTS.iter().any(|(cn, en)| {
        text.contains(cn) || lower.contains(&en.to_lowercase())
    })
}

/// 解析模型目录：配置里的路径优先，其次 `data/models/clap`，最后 `exe/models/clap`。
pub fn resolve_dir(cfg: &crate::config::ModelConfig, app_dir: &Path, data_dir: &Path) -> Option<PathBuf> {
    if !cfg.clap_enabled {
        return None;
    }
    let custom = cfg.clap_dir.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        return if p.join("audio_model.onnx").exists() { Some(p) } else { None };
    }
    let a = data_dir.join("models").join("clap");
    if a.join("audio_model.onnx").exists() {
        return Some(a);
    }
    let b = app_dir.join("models").join("clap");
    if b.join("audio_model.onnx").exists() {
        return Some(b);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::audio_windows;

    #[test]
    fn clap_windows_cover_the_full_duration() {
        assert_eq!(audio_windows(0.0), Vec::<(f64, f64)>::new());
        assert_eq!(audio_windows(7.5), vec![(0.0, 7.5)]);
        assert_eq!(audio_windows(25.0), vec![(0.0, 10.0), (10.0, 10.0), (20.0, 5.0)]);
    }

    #[test]
    fn clap_window_tail_is_never_zero_length() {
        let windows = audio_windows(20.01);
        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|(_, length)| *length >= 0.25));
        let end = windows
            .iter()
            .map(|(start, length)| start + length)
            .fold(0.0, f64::max);
        assert!(end >= 20.01);
    }
}
