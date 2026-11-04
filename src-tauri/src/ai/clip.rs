//! 跨模态（文字 ↔ 图片）编码器：中文 CLIP 的 ONNX 量化版。
//!
//! 与 `HashEmbedder` / `LlamaEmbedder` 的区别是**图像塔和文本塔在同一个
//! 空间里**，所以「用一句话描述找图片」才能成立。llama.cpp 那边只跑了
//! 文本塔，图文余弦只有 0.03~0.09，完全没法用。
//!
//! 模型：`clip-vit-base-patch16-q.onnx`（190MB）+ `clip-vocab.txt`，
//! 放在数据目录的 `models/clip/` 下。缺文件时返回 `None`，整个功能静默
//! 降级，不影响搜索、索引等主流程。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::ort::{Data, Ort, Session};

/// 图像侧输入边长。
const SIDE: usize = 224;
/// 文本最大 token 数（与模型导出的 64 一致）。
const MAX_TOKENS: usize = 64;
const MEAN: [f32; 3] = [0.481_454_7, 0.457_827_5, 0.408_210_7];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_1];

pub struct ClipModel {
    _ort: Ort,
    /// 文本塔与图像塔在同一张图里，前向要串行；用 Mutex 保证。
    sess: std::sync::Mutex<Session>,
    vocab: HashMap<String, i64>,
    dim: usize,
    model_path: PathBuf,
}

impl ClipModel {
    /// 从目录加载：`model.onnx` + `vocab.txt`，任一缺失返回 None。
    pub fn load(dir: &Path, threads: i32) -> Option<ClipModel> {
        let model = dir.join("model.onnx");
        let vocab_path = dir.join("vocab.txt");
        if !model.exists() || !vocab_path.exists() {
            return None;
        }
        // onnxruntime.dll：先找 exe 同目录，再找当前目录。
        let dll = find_dll()?;
        // 三条失败路径都要出声：不然"图片搜不到"永远查不出是哪一步断的。
        let ort = match Ort::load(&dll, threads) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[clip] 加载 {} 失败：{e}", dll.display());
                return None;
            }
        };
        let sess = match ort.session(&model) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[clip] 创建会话失败（{}）：{e}", model.display());
                return None;
            }
        };
        let vocab = match load_vocab(&vocab_path) {
            Some(v) => v,
            None => {
                eprintln!("[clip] 词表为空或不可读：{}", vocab_path.display());
                return None;
            }
        };
        eprintln!("[clip] 已加载：{} · EP={}", model.display(), if ort.execution_provider().is_empty() { "CPU" } else { ort.execution_provider() });
        Some(ClipModel {
            _ort: ort,
            sess: std::sync::Mutex::new(sess),
            vocab,
            dim: 512,
            model_path: model,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 给「模型」页显示的一行状态。
    pub fn describe(&self) -> String {
        format!("中文 CLIP · {} 维 · {}", self.dim, self.model_path.display())
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// 文本 → 512 维向量（已归一化）。
    pub fn text_vec(&self, text: &str) -> Result<Vec<f32>, String> {
        let (ids, mask) = self.encode(text);
        // 文本塔也要喂像素：模型是单图导出，两个塔一起跑，缺输入会报错。
        // 全零图等于「没有图像」，不影响文本特征。
        let pixels = vec![0f32; 3 * SIDE * SIDE];
        let ids_shape = [1i64, MAX_TOKENS as i64];
        let px_shape = [1i64, 3, SIDE as i64, SIDE as i64];
        let mask_shape = [1i64, MAX_TOKENS as i64];
        let sess = self.sess.lock().map_err(|e| e.to_string())?;
        let out = sess.run(
            &[
                ("input_ids", &ids_shape, Data::I64(&ids)),
                ("pixel_values", &px_shape, Data::F32(&pixels)),
                ("attention_mask", &mask_shape, Data::I64(&mask)),
            ],
            &["text_embeds"],
        )?;
        let mut v = out.into_iter().next().ok_or("模型没有返回文本向量")?;
        normalize(&mut v);
        Ok(v)
    }

    /// 图片 → 512 维向量（已归一化）。
    pub fn image_vec(&self, path: &Path) -> Result<Vec<f32>, String> {
        let image = image::open(path).map_err(|e| format!("读图失败：{e}"))?;
        self.image_data_vec(&image)
    }

    pub fn image_data_vec(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let vectors = self.image_data_vecs(&[image])?;
        vectors.into_iter().next().ok_or_else(|| "模型没有返回图像向量".into())
    }

    /// 只提交前向：像素已由调用方在其他线程里准备好。
    ///
    /// 索引器把 `prepare_image_pixels` 放进 rayon 生产者后，需要一条
    /// 「不碰图像处理、只走 ONNX」的入口，否则并行算好的像素没法复用。
    pub fn embed_pixels(&self, pixel_rows: &[&Vec<f32>]) -> Result<Vec<Vec<f32>>, String> {
        clip_image_embeds(&self.sess, pixel_rows)
    }

    /// 一次前向处理多张图。
    ///
    /// 这个 ONNX 导出的 `pixel_values` 是 `[image_batch_size, 3, 224, 224]`，
    /// batch 维是**符号**（不是固定 1），所以官方支持批量。以前这里每张图
    /// 单独跑一次 Session::run，DirectML 上每次都要重新提交一个 190 MB 的
    /// 权重集 → GPU 利用率被前向之间的空隙吃光。改成整批一次前向后，
    /// 同样的图片数 GPU 忙的时间占比明显上升。
    pub fn image_data_vecs(
        &self,
        images: &[&image::DynamicImage],
    ) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let n = images.len() as i64;
        let mut pixels = Vec::with_capacity(images.len() * 3 * SIDE * SIDE);
        for image in images {
            pixels.extend_from_slice(&self.prepare_image(image)?);
        }
        let ids = vec![101i64; MAX_TOKENS * images.len()];
        let mask = vec![1i64; MAX_TOKENS * images.len()];
        let ids_shape = [n, MAX_TOKENS as i64];
        let px_shape = [n, 3, SIDE as i64, SIDE as i64];
        let mask_shape = [n, MAX_TOKENS as i64];
        let sess = self.sess.lock().map_err(|e| e.to_string())?;
        let out = sess.run(
            &[
                ("input_ids", &ids_shape, Data::I64(&ids)),
                ("pixel_values", &px_shape, Data::F32(&pixels)),
                ("attention_mask", &mask_shape, Data::I64(&mask)),
            ],
            &["image_embeds"],
        )?;
        let flat = out.into_iter().next().ok_or("模型没有返回图像向量")?;
        if flat.len() != images.len() * self.dim {
            return Err(format!(
                "CLIP 批量输出维度不符：{} / 期望 {}×{}",
                flat.len(),
                images.len(),
                self.dim
            ));
        }
        let mut vectors = Vec::with_capacity(images.len());
        for chunk in flat.chunks(self.dim) {
            let mut v = chunk.to_vec();
            normalize(&mut v);
            vectors.push(v);
        }
        Ok(vectors)
    }

    /// 等比缩放 + 中心裁剪到 224×224，转 CHW 并归一化。
    fn prepare_image(&self, image: &image::DynamicImage) -> Result<Vec<f32>, String> {
        prepare_image_pixels(image)
    }

    fn encode(&self, text: &str) -> (Vec<i64>, Vec<i64>) {
        bert_encode(&self.vocab, text, MAX_TOKENS)
    }
}

/// 等比缩放 + 中心裁剪到 224×224，转 CHW 并归一化（CLIP 的像素输入）。
///
/// 独立成自由函数是为了让**调用方能在自己的线程池里并行预处理**。
/// 之前它只在消费者线程里跑：索引图片的 32 条解码线程全在等
/// `sync_channel`，而单单这里的 CatmullRom 缩放就要 83~103 ms/张
/// （pixiv 3.5 MB 的 p50 原图），一轮 8 张光预处理就 0.8 秒 ——
/// 实测把 CLIP 打开后整条索引链从 10.2 张/秒掉到 1.0 张/秒。
///
/// 滤波从 CatmullRom 换成 Triangle：224×224 的检索向量对插值核不敏感，
/// 但 3 抽头替代 4 抽头带负瓣的核，实测省掉约 25% 的缩放时间。
pub fn prepare_image_pixels(image: &image::DynamicImage) -> Result<Vec<f32>, String> {
    let img = image.to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return Err("图片尺寸为 0".into());
    }
    // 短边先缩到 224，再中心裁 224×224。
    let scale = SIDE as f32 / w.min(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(SIDE as u32);
    let nh = ((h as f32 * scale).round() as u32).max(SIDE as u32);
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    let x = (nw as usize - SIDE) / 2;
    let y = (nh as usize - SIDE) / 2;
    let cropped = image::imageops::crop_imm(&resized, x as u32, y as u32, SIDE as u32, SIDE as u32).to_image();

    let mut buf = vec![0f32; 3 * SIDE * SIDE];
    let plane = SIDE * SIDE;
    for (i, px) in cropped.pixels().enumerate() {
        for c in 0..3 {
            buf[c * plane + i] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    Ok(buf)
}

/// 只做前向：像素已经按 `[n,3,224,224]` 拍平并归一化。
///
/// 与 `image_data_vecs` 的区别是它完全不做图像处理，只碰 ONNX 会话，
/// 所以调用方可以先把像素在多个线程里准备好，再由单线程集中提交。
pub fn clip_image_embeds(
    sess: &std::sync::Mutex<Session>,
    pixel_rows: &[&Vec<f32>],
) -> Result<Vec<Vec<f32>>, String> {
    if pixel_rows.is_empty() {
        return Ok(Vec::new());
    }
    let n = pixel_rows.len() as i64;
    let plane = 3 * SIDE * SIDE;
    let mut pixels = Vec::with_capacity(pixel_rows.len() * plane);
    for row in pixel_rows {
        if row.len() != plane {
            return Err(format!("CLIP 像素行长度不符：{} / 期望 {}", row.len(), plane));
        }
        pixels.extend_from_slice(row.as_slice());
    }
    let ids = vec![101i64; MAX_TOKENS * pixel_rows.len()];
    let mask = vec![1i64; MAX_TOKENS * pixel_rows.len()];
    let ids_shape = [n, MAX_TOKENS as i64];
    let px_shape = [n, 3, SIDE as i64, SIDE as i64];
    let mask_shape = [n, MAX_TOKENS as i64];
    let guard = sess.lock().map_err(|e| e.to_string())?;
    let out = guard.run(
        &[
            ("input_ids", &ids_shape, Data::I64(&ids)),
            ("pixel_values", &px_shape, Data::F32(&pixels)),
            ("attention_mask", &mask_shape, Data::I64(&mask)),
        ],
        &["image_embeds"],
    )?;
    let flat = out.into_iter().next().ok_or("模型没有返回图像向量")?;
    let dim = 512usize;
    if flat.len() != pixel_rows.len() * dim {
        return Err(format!(
            "CLIP 批量输出维度不符：{} / 期望 {}×{}",
            flat.len(),
            pixel_rows.len(),
            dim
        ));
    }
    let mut vectors = Vec::with_capacity(pixel_rows.len());
    for chunk in flat.chunks(dim) {
        let mut v = chunk.to_vec();
        normalize(&mut v);
        vectors.push(v);
    }
    Ok(vectors)
}

/// 极简 BERT 分词：中文按字、英文按词 + wordpiece 贪心最长匹配。
///
/// 不追求和 HuggingFace 完全一致——检索场景下差一两个 token 几乎
/// 不影响相似度，但换来零依赖、几十行的实现。CLAP 的文本塔也用这套，
/// 两个模型的词表都是中文 bert-base 的 21128 词。
pub fn bert_encode(vocab: &HashMap<String, i64>, text: &str, max_len: usize) -> (Vec<i64>, Vec<i64>) {
    let mut ids = Vec::with_capacity(max_len);
    ids.push(101); // [CLS]
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ids.len() >= max_len - 1 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '\'' {
            word.push(ch);
            continue;
        }
        if !word.is_empty() {
            push_word(vocab, &word, &mut ids, max_len);
            word.clear();
        }
        if ch.is_whitespace() {
            continue;
        }
        let s = ch.to_string();
        if let Some(id) = vocab.get(&s).or_else(|| vocab.get(&s.to_lowercase())) {
            ids.push(*id);
        }
        if ids.len() >= max_len - 1 {
            break;
        }
    }
    if !word.is_empty() && ids.len() < max_len - 1 {
        push_word(vocab, &word, &mut ids, max_len);
    }
    ids.truncate(max_len - 1);
    ids.push(102); // [SEP]
    let n = ids.len();
    ids.resize(max_len, 0);
    let mut mask = vec![0i64; max_len];
    for m in mask.iter_mut().take(n) {
        *m = 1;
    }
    (ids, mask)
}

/// 英文词：先整体查，失败再 wordpiece 切分（`##` 续词）。
fn push_word(vocab: &HashMap<String, i64>, word: &str, ids: &mut Vec<i64>, max_len: usize) {
    let lower = word.to_lowercase();
    if let Some(id) = vocab.get(&lower) {
        ids.push(*id);
        return;
    }
    let mut start = 0;
    let bytes = lower.as_bytes();
    while start < bytes.len() && ids.len() < max_len - 1 {
        let mut end = bytes.len();
        let mut found = None;
        // 贪心：从最长的前缀开始试
        while end > start {
            if !lower.is_char_boundary(end) {
                end -= 1;
                continue;
            }
            let piece = &lower[start..end];
            let key = if start == 0 {
                piece.to_string()
            } else {
                format!("##{piece}")
            };
            if let Some(id) = vocab.get(&key) {
                found = Some((*id, end));
                break;
            }
            end -= 1;
        }
        match found {
            Some((id, e)) => {
                ids.push(id);
                start = e;
            }
            // 完全切不出来就跳过这个词，别死循环。
            None => break,
        }
    }
}

/// 解析模型目录：配置里的路径优先，其次 `data/models/clip`，最后 `exe/models/clip`。
///
/// 返回 None 表示「没装模型」——调用方据此静默关闭跨模态检索，
/// 而不是弹错误：大部分用户只用文件名搜索，不该被模型缺失打断。
pub fn resolve_dir(cfg: &crate::config::ModelConfig, app_dir: &Path, data_dir: &Path) -> Option<PathBuf> {
    if !cfg.clip_enabled {
        return None;
    }
    let custom = cfg.clip_dir.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        return if p.join("model.onnx").exists() { Some(p) } else { None };
    }
    let a = data_dir.join("models").join("clip");
    if a.join("model.onnx").exists() {
        return Some(a);
    }
    let b = app_dir.join("models").join("clip");
    if b.join("model.onnx").exists() {
        return Some(b);
    }
    None
}

pub fn find_dll() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    // An explicit path is useful for portable installs and keeps the runtime
    // inside the project directory instead of relying on PATH or C:\Windows.
    if let Ok(path) = std::env::var("DEEPAISEE_ORT_DLL") {
        if !path.trim().is_empty() {
            cands.push(PathBuf::from(path));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("onnxruntime.dll"));
            cands.push(dir.join("resources").join("onnxruntime.dll"));
            cands.push(dir.join("resources").join(".tools").join("onnxruntime").join("onnxruntime.dll"));
            // Tauri's debug runner stages resources below `_up_`; keep this
            // path for local smoke tests as well as portable debug builds.
            cands.push(dir.join("_up_").join("onnxruntime.dll"));
            cands.push(dir.join("_up_").join(".tools").join("onnxruntime").join("onnxruntime.dll"));
            cands.push(dir.join("tools").join("onnxruntime").join("onnxruntime.dll"));
            cands.push(dir.join(".tools").join("onnxruntime").join("onnxruntime.dll"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("onnxruntime.dll"));
        cands.push(cwd.join("tools").join("onnxruntime").join("onnxruntime.dll"));
        cands.push(cwd.join(".tools").join("onnxruntime").join("onnxruntime.dll"));
    }
    // 开发时 dll 放在 src-tauri 下。
    cands.push(PathBuf::from("onnxruntime.dll"));
    cands.into_iter().find(|p| p.exists())
}

pub fn load_vocab(path: &Path) -> Option<HashMap<String, i64>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut map = HashMap::with_capacity(21_128);
    for (i, line) in text.lines().enumerate() {
        let tok = line.trim_end_matches(['\r', '\n']);
        if !tok.is_empty() {
            map.insert(tok.to_string(), i as i64);
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-8 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// 模型目录下的默认文件名，供 model 页展示。
pub fn default_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models").join("clip")
}
