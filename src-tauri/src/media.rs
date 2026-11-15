//! 媒体处理：用内置 ffmpeg 抽帧/解码音频，生成缩略图。
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 找 ffmpeg：优先 exe 同级的 tools/bin/ffmpeg.exe，其次 PATH。
pub fn ffmpeg_path() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        // 从 exe 所在目录逐级向上找 4 层：debug 版 exe 在 src-tauri/target/debug，
        // 打包版在 <app>/ 下，两种情况 tools/bin 都可能在更上层
        for _ in 0..4 {
            let Some(d) = dir else { break };
            cands.push(d.join("tools").join("bin").join("ffmpeg.exe"));
            cands.push(d.join("ffmpeg.exe"));
            cands.push(d.join("resources").join("ffmpeg.exe"));
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = Some(cwd);
        for _ in 0..4 {
            let Some(d) = dir else { break };
            cands.push(d.join("tools").join("bin").join("ffmpeg.exe"));
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    for c in cands {
        if c.is_file() {
            return Some(c);
        }
    }
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        return Some(PathBuf::from("ffmpeg"));
    }
    None
}

fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn ffmpeg_cuda_decode_available(ff: &Path) -> bool {
    static CUDA_AVAILABLE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CUDA_AVAILABLE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = ff.to_path_buf();
    if let Ok(guard) = cache.lock() {
        if let Some(value) = guard.get(&key) {
            return *value;
        }
    }
    let available = {
        let Ok(output) = hidden(
            Command::new(ff)
                .args(["-hide_banner", "-hwaccels"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .output() else {
            return false;
        };
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        text.lines()
            .any(|line| line.trim().eq_ignore_ascii_case("cuda"))
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, available);
    }
    available
}

/// Wait for an FFmpeg process while periodically checking whether the owning
/// indexing operation was cancelled. Drain both pipes concurrently so raw
/// audio output cannot fill the OS pipe buffer and stall the child.
fn wait_child_with_cancel<F>(mut child: Child, cancelled: &F) -> std::io::Result<Output>
where
    F: Fn() -> bool,
{
    use std::io::Read;

    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });

    let status = loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "media operation cancelled",
            ));
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };

    let collect = |pipe: Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>| {
        pipe.map(|handle| {
            handle
                .join()
                .map_err(|_| std::io::Error::other("FFmpeg pipe reader panicked"))?
        })
        .transpose()
        .map(|bytes| bytes.unwrap_or_default())
    };
    Ok(Output {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    })
}

fn run_video_ffmpeg_with_cancel<F, C>(
    ff: &Path,
    mut build: F,
    cancelled: &C,
) -> std::io::Result<Output>
where
    F: FnMut(bool) -> Command,
    C: Fn() -> bool,
{
    static CUDA_RUNTIME_DISABLED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let disabled = CUDA_RUNTIME_DISABLED.get_or_init(|| Mutex::new(HashSet::new()));
    let runtime_disabled = disabled
        .lock()
        .ok()
        .is_some_and(|guard| guard.contains(ff));
    if cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "media operation cancelled",
        ));
    }
    let use_cuda = !runtime_disabled && ffmpeg_cuda_decode_available(ff);
    let first = wait_child_with_cancel(hidden(&mut build(use_cuda)).spawn()?, cancelled)?;
    if first.status.success() || !use_cuda {
        return Ok(first);
    }
    // A CUDA-enabled FFmpeg binary can still fail at runtime when the driver
    // or decoder is unavailable. Remember that state for this process so a
    // long video does not pay the failed CUDA startup cost for every window.
    // Codec-specific decode errors are intentionally left as one-off failures;
    // only clear runtime/device errors trip the circuit breaker.
    let stderr = String::from_utf8_lossy(&first.stderr).to_ascii_lowercase();
    if stderr.contains("cannot load nvcuda")
        || stderr.contains("no device")
        || stderr.contains("cuda error")
        || stderr.contains("cuinit")
        || (stderr.contains("hwaccel") && stderr.contains("failed"))
        || (stderr.contains("hwaccel_output_format")
            && (stderr.contains("unrecognized") || stderr.contains("unknown option")))
    {
        if let Ok(mut guard) = disabled.lock() {
            guard.insert(ff.to_path_buf());
        }
    }
    if cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "media operation cancelled",
        ));
    }
    wait_child_with_cancel(hidden(&mut build(false)).spawn()?, cancelled)
}

fn run_video_ffmpeg<F>(ff: &Path, build: F) -> std::io::Result<Output>
where
    F: FnMut(bool) -> Command,
{
    run_video_ffmpeg_with_cancel(ff, build, &|| false)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimedSubtitle {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
}

fn subtitle_timestamp(value: &str) -> Option<f64> {
    let value = value.trim().replace(',', ".");
    let parts: Vec<_> = value.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (
            0.0,
            minutes.parse::<f64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<f64>().ok()?,
            minutes.parse::<f64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        _ => return None,
    };
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    (total.is_finite() && total >= 0.0).then_some(total)
}

fn clean_subtitle_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
        .replace("\\N", " ")
        .replace("\\n", " ")
        .replace("\\h", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_subtitle_timestamp_range(line: &str) -> Option<(f64, f64)> {
    let (start, end) = line.split_once("-->")?;
    let end = end.split_whitespace().next()?;
    let start = subtitle_timestamp(start)?;
    let end = subtitle_timestamp(end)?;
    (end >= start).then_some((start, end))
}

pub fn parse_subtitle_text(text: &str) -> Vec<TimedSubtitle> {
    let mut cues = Vec::new();
    let mut current: Option<(f64, f64, Vec<String>)> = None;
    let flush = |current: &mut Option<(f64, f64, Vec<String>)>, cues: &mut Vec<TimedSubtitle>| {
        if let Some((start_seconds, end_seconds, lines)) = current.take() {
            let text = clean_subtitle_text(&lines.join(" "));
            if !text.is_empty() {
                cues.push(TimedSubtitle {
                    start_seconds,
                    end_seconds,
                    text,
                });
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if let Some((start, end)) = parse_subtitle_timestamp_range(trimmed) {
            flush(&mut current, &mut cues);
            current = Some((start, end, Vec::new()));
        } else if let Some(rest) = trimmed.strip_prefix("Dialogue:") {
            flush(&mut current, &mut cues);
            // ASS has nine comma-delimited metadata fields before its text.
            let mut fields = rest.trim().splitn(10, ',');
            let fields: Vec<_> = fields.by_ref().collect();
            if fields.len() == 10 {
                if let (Some(start), Some(end)) =
                    (subtitle_timestamp(fields[1]), subtitle_timestamp(fields[2]))
                {
                    let content = clean_subtitle_text(fields[9]);
                    if !content.is_empty() && end >= start {
                        cues.push(TimedSubtitle {
                            start_seconds: start,
                            end_seconds: end,
                            text: content,
                        });
                    }
                }
            }
        } else if let Some((_, _, lines)) = current.as_mut() {
            if !trimmed.is_empty()
                && !trimmed.starts_with("WEBVTT")
                && !trimmed.chars().all(|ch| ch.is_ascii_digit())
            {
                lines.push(trimmed.to_string());
            }
        }
    }
    flush(&mut current, &mut cues);
    cues.sort_by(|a, b| {
        a.start_seconds
            .partial_cmp(&b.start_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cues
}

fn ffprobe_path(ffmpeg: &Path) -> Option<PathBuf> {
    if let Some(parent) = ffmpeg.parent() {
        let name = if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        };
        let sibling = parent.join(name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let candidate = if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    };
    Command::new(candidate)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|status| status.success())
        .map(|_| PathBuf::from(candidate))
}

fn is_text_subtitle_codec(codec: &str) -> bool {
    !matches!(
        codec.to_ascii_lowercase().as_str(),
        "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext"
    )
}

fn embedded_subtitles(path: &Path, ffmpeg: &Path) -> Vec<TimedSubtitle> {
    let streams = if let Some(probe) = ffprobe_path(ffmpeg) {
        let output = match hidden(
            Command::new(probe)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "s",
                    "-show_entries",
                    "stream=index,codec_name",
                    "-of",
                    "json",
                ])
                .arg(path)
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .output()
        {
            Ok(output) if output.status.success() => output,
            _ => return Vec::new(),
        };
        let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        parsed
            .get("streams")
            .and_then(serde_json::Value::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|stream| {
                        Some((
                            stream.get("index")?.as_u64()? as usize,
                            stream.get("codec_name")?.as_str()?.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        let output = match hidden(
            Command::new(ffmpeg)
                .args(["-hide_banner", "-i"])
                .arg(path)
                .args(["-map", "0:s", "-c", "copy", "-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped()),
        )
        .output()
        {
            Ok(output) => output,
            Err(_) => return Vec::new(),
        };
        parse_ffmpeg_subtitle_streams(&String::from_utf8_lossy(&output.stderr))
    };
    let mut cues = Vec::new();
    for (index, codec) in streams {
        if !is_text_subtitle_codec(&codec) {
            continue;
        }
        let output = hidden(
            Command::new(ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(path)
                .args([
                    "-map",
                    &format!("0:{index}"),
                    "-c:s",
                    "srt",
                    "-f",
                    "srt",
                    "-",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .output();
        if let Ok(output) = output {
            if output.status.success() {
                cues.extend(parse_subtitle_text(&String::from_utf8_lossy(
                    &output.stdout,
                )));
            }
        }
    }
    cues
}

fn parse_ffmpeg_subtitle_streams(text: &str) -> Vec<(usize, String)> {
    let mut streams = Vec::new();
    for line in text.lines() {
        let Some(marker) = line.find("Stream #0:") else {
            continue;
        };
        let rest = &line[marker + "Stream #0:".len()..];
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        let Ok(index) = digits.parse::<usize>() else {
            continue;
        };
        let Some(subtitle_pos) = rest.find("Subtitle:") else {
            continue;
        };
        let codec = rest[subtitle_pos + "Subtitle:".len()..]
            .split(|ch: char| ch == ',' || ch == '(')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        streams.push((index, codec));
    }
    streams
}

/// Load sibling subtitle files and any extractable embedded text subtitle
/// streams. Bitmap subtitle formats are intentionally skipped because they
/// require OCR and cannot be losslessly converted by FFmpeg to SRT.
pub fn video_subtitles(path: &Path) -> Vec<TimedSubtitle> {
    let mut cues = Vec::new();
    for extension in ["srt", "vtt", "ass", "ssa"] {
        let sibling = path.with_extension(extension);
        if let Ok(text) = std::fs::read_to_string(sibling) {
            cues.extend(parse_subtitle_text(&text));
        }
    }
    if let Some(ffmpeg) = ffmpeg_path() {
        cues.extend(embedded_subtitles(path, &ffmpeg));
    }
    cues.sort_by(|a, b| {
        a.start_seconds
            .partial_cmp(&b.start_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cues.dedup_by(|a, b| {
        (a.start_seconds - b.start_seconds).abs() < 0.05
            && (a.end_seconds - b.end_seconds).abs() < 0.05
            && a.text == b.text
    });
    cues
}

pub fn subtitle_text_for_window(
    cues: &[TimedSubtitle],
    start_seconds: f64,
    end_seconds: f64,
) -> String {
    subtitle_cues_for_window(cues, start_seconds, end_seconds)
        .iter()
        .map(|cue| cue.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Return subtitle cues that overlap a temporal media window.
///
/// Keeping cue boundaries available lets callers preserve the exact timestamp
/// inside a 30-second embedding window without changing the persisted video
/// segment schema. A cue that starts before a window but remains visible in it
/// is intentionally included as well.
pub fn subtitle_cues_for_window(
    cues: &[TimedSubtitle],
    start_seconds: f64,
    end_seconds: f64,
) -> Vec<TimedSubtitle> {
    if !start_seconds.is_finite() || !end_seconds.is_finite() || end_seconds <= start_seconds {
        return Vec::new();
    }
    cues.iter()
        .filter(|cue| cue.end_seconds > start_seconds && cue.start_seconds < end_seconds)
        .cloned()
        .collect()
}

fn format_subtitle_timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let hours = (seconds / 3600.0).floor() as u64;
    let minutes = ((seconds % 3600.0) / 60.0).floor() as u64;
    let whole_seconds = seconds.floor() as u64 % 60;
    let millis = ((seconds.fract().max(0.0)) * 1000.0).round() as u64;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{whole_seconds:02}.{millis:03}")
    } else {
        format!("{minutes:02}:{whole_seconds:02}.{millis:03}")
    }
}

/// Render overlapping cues with their original time ranges.
///
/// The resulting text remains valid input for the text encoder while making
/// the precise cue time searchable and available to the result label. The old
/// `subtitle_text_for_window` function deliberately remains unchanged for
/// callers that need plain subtitle text.
pub fn subtitle_text_for_window_with_timestamps(
    cues: &[TimedSubtitle],
    start_seconds: f64,
    end_seconds: f64,
) -> String {
    subtitle_cues_for_window(cues, start_seconds, end_seconds)
        .into_iter()
        .map(|cue| {
            format!(
                "[{}-{}] {}",
                format_subtitle_timestamp(cue.start_seconds),
                format_subtitle_timestamp(cue.end_seconds),
                cue.text
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 临时工作目录：优先 exe 同级的 `data/tmp`，退回系统 %TEMP%。
///
/// 为什么要自己找：有些机器的 %TEMP%（漫游配置文件损坏 / 安全软件锁目录）
/// 建不了子目录，抽帧命令会静默失败，表现为「视频永远索引不出向量」——
/// 用户完全看不出原因。放在自己的数据目录下则一定能写。
fn work_dir() -> PathBuf {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join("data").join("tmp"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("data").join("tmp"));
    }
    cands.push(std::env::temp_dir().join("DeepDark_Oracle"));
    for d in cands {
        if std::fs::create_dir_all(&d).is_ok() {
            return d;
        }
    }
    std::env::temp_dir()
}

/// 抽视频帧到临时目录，返回图片路径列表。
pub fn sample_video_frames(path: &Path, count: usize, max_side: u32) -> Vec<PathBuf> {
    let Some(ff) = ffmpeg_path() else {
        return vec![];
    };
    let base = work_dir();
    let tmp = base.join(format!("frames-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        return vec![];
    }
    let mut out = Vec::new();
    // 先取时长
    let Some(dur) = video_duration(path) else {
        return Vec::new();
    };
    let n = count.max(1);
    for i in 0..n {
        let t = dur * (i as f64 + 0.5) / n as f64;
        let dest = tmp.join(format!("f{i}.jpg"));
        let scale = format!(
            "scale='min({},iw)':'min({},ih)':force_original_aspect_ratio=decrease",
            max_side, max_side
        );
        let ok = hidden(
            Command::new(&ff)
                .args(["-y", "-ss", &format!("{t:.3}"), "-i"])
                .arg(path)
                .args(["-frames:v", "1", "-vf", &scale, "-q:v", "3"])
                .arg(&dest)
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
        if ok && dest.is_file() {
            out.push(dest);
        }
    }
    if out.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    out
}

/// 智能抽帧参数。需求：按时间间隔抽帧（默认至少 4 秒一帧）、I 帧优先、
/// 场景切换优先、图像指纹去重、总帧数封顶。
#[derive(Debug, Clone, Copy)]
pub struct FramePlan {
    /// 相邻采样点的时间间隔（秒）。1 秒对应 EmbeddingGemma2 的默认 1 FPS。
    pub interval: f32,
    /// 单个视频最多保留多少帧。
    pub max_frames: u32,
    /// 输出图片的最长边。
    pub max_side: u32,
    /// dHash 汉明距离小于等于该值时视为同一画面丢弃。
    pub dedup_distance: u32,
}

impl Default for FramePlan {
    fn default() -> Self {
        Self {
            interval: 1.0,
            max_frames: 64,
            max_side: 256,
            dedup_distance: 6,
        }
    }
}

/// 从 ffmpeg 的 showinfo 输出里抓所有 `pts_time:` 后面的秒数。
fn parse_showinfo_times(text: &str) -> Vec<f64> {
    let mut times = Vec::new();
    for line in text.lines() {
        // [Parsed_showinfo_1 @ ...] n:   0 pts:      0 pts_time:0       duration:...
        let Some(pos) = line.find("pts_time:") else {
            continue;
        };
        let rest = &line[pos + 9..];
        let value: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if let Ok(seconds) = value.parse::<f64>() {
            if seconds.is_finite() && seconds >= 0.0 {
                times.push(seconds);
            }
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    times
}

/// 视频关键帧（I 帧）时间戳列表，秒。
///
/// 用 `-skip_frame nokey` 只解码关键帧，长视频也能在一两秒内跑完；
/// 关键帧天然落在场景切换处，既满足「I 帧优先」，又顺带拿到场景变化点。
/// 失败时返回空列表，调用方退回均匀采样（不能因为探测失败就不抽帧）。
fn video_keyframe_times(path: &Path) -> Vec<f64> {
    let Some(ff) = ffmpeg_path() else {
        return vec![];
    };
    let out = run_video_ffmpeg(&ff, |cuda| {
        let mut command = Command::new(&ff);
        command.args(["-hide_banner", "-nostats", "-loglevel", "info"]);
        if cuda {
            command.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
        }
        command.args(["-skip_frame", "nokey", "-i"]).arg(path);
        let filter = if cuda {
            "hwdownload,format=nv12,select=eq(pict_type\\,I),showinfo"
        } else {
            "select=eq(pict_type\\,I),showinfo"
        };
        command.args(["-vf", filter, "-an", "-f", "null", "-"]);
        command
    });
    let Ok(out) = out else { return vec![] };
    parse_showinfo_times(&String::from_utf8_lossy(&out.stderr))
}

/// 场景切换时间戳（Scene Change Detection），秒。
///
/// 需求里的「动作片提高采样率、PPT/网课降采样」就靠它判断：
/// ffmpeg 的 `scene` 分数是相邻帧的直方图差异，超过阈值即视为换镜头。
///
/// 为控制开销做了三件事：先 `fps=2` 降到每秒 2 帧再算（scene 分数只看相邻帧，
/// 降帧后差异只会更明显，阈值相应取 0.4）；缩到 160 宽；超过 40 分钟直接跳过
/// （长视频仍然有 I 帧可以挑，不影响「I 帧优先」）。
fn video_scene_change_times(path: &Path, dur: f64) -> Vec<f64> {
    if dur > 2400.0 {
        return vec![];
    }
    let Some(ff) = ffmpeg_path() else {
        return vec![];
    };
    let out = run_video_ffmpeg(&ff, |cuda| {
        let mut command = Command::new(&ff);
        command.args(["-hide_banner", "-nostats", "-loglevel", "info"]);
        if cuda {
            command.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
        }
        command.arg("-i").arg(path);
        let filter = if cuda {
            "hwdownload,format=nv12,fps=2,scale=160:-2,select='gt(scene\\,0.4)',showinfo"
        } else {
            "fps=2,scale=160:-2,select='gt(scene\\,0.4)',showinfo"
        };
        command.args(["-vf", filter, "-an", "-f", "null", "-"]);
        command
    });
    let Ok(out) = out else { return vec![] };
    parse_showinfo_times(&String::from_utf8_lossy(&out.stderr))
}

/// 按场景切换密度自适应调整采样间隔（秒）。
///
/// - 几乎不换镜头（PPT / 网课 / 录屏）：放稀到 1.5 倍，省 CLIP 计算量与磁盘；
/// - 换镜头很频繁（动作片 / 体育）：缩到 0.5 倍，尽量不漏关键画面；
/// - 中间档保持配置值。
///
/// 采样密度由专业配置控制，帧数上限负责约束长视频的成本。
fn adaptive_interval(base: f64, dur: f64, scene_count: usize) -> f64 {
    if dur < 1.0 {
        return base.max(0.1);
    }
    if scene_count == 0 {
        return (base * 1.5).max(0.1);
    }
    let per_minute = scene_count as f64 * 60.0 / dur;
    if per_minute >= 8.0 {
        return (base * 0.5).max(0.1);
    }
    base.max(0.1)
}

/// 在指定时间点抽一帧到 `dest`。
fn grab_frame(ff: &Path, src: &Path, seconds: f64, max_side: u32, dest: &Path) -> bool {
    let scale = format!(
        "scale='min({m},iw)':'min({m},ih)':force_original_aspect_ratio=decrease",
        m = max_side
    );
    run_video_ffmpeg(ff, |cuda| {
        let mut command = Command::new(ff);
        command.args(["-y", "-ss", &format!("{seconds:.3}")]);
        if cuda {
            command.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
        }
        command.arg("-i").arg(src);
        let filter = if cuda {
            format!("hwdownload,format=nv12,{scale}")
        } else {
            scale.clone()
        };
        command
            .args(["-frames:v", "1", "-vf", &filter, "-q:v", "4"])
            .arg(dest)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    })
    .map(|output| output.status.success())
    .unwrap_or(false)
        && dest.is_file()
}

/// 图像指纹（dHash）：缩到 9x8 灰度，比较相邻像素亮度，得到 64 位指纹。
///
/// 不用 phash 是为了零依赖、零调参：dHash 对缩放/轻微压缩不敏感，
/// 对画面变化敏感，正好用来丢弃「同一镜头里的连续帧」。
pub fn dhash(path: &Path) -> Option<u64> {
    frame_signature(path).map(|sig| sig.0)
}

/// 帧特征：dHash 指纹 + 平均颜色。
///
/// 只用 dHash 时，纯色 / 极低对比的画面（幻灯片白底、黑屏、纯色转场）
/// 几乎都会得到同一个指纹，"红底切绿底" 会被误判成同一帧直接丢掉。
/// 带上平均颜色就能区分这类切换 —— 两帧既要求结构相近、又要求颜色相近
/// 才算重复。
fn frame_signature(path: &Path) -> Option<(u64, [u8; 3])> {
    let img = image::open(path).ok()?;
    let small = img
        .resize_exact(9, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut hash = 0u64;
    let mut bit = 0;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            if left > right {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    let mean = img
        .resize_exact(1, 1, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let rgb = mean.get_pixel(0, 0).0;
    Some((hash, rgb))
}

fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 两帧平均颜色的距离（RGB 三分量差的平方和，满值 195075）。
/// 阈值取 300 相当于每通道平均差 ~10/255，轻微偏色不算换画面。
fn color_distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = *x as i32 - *y as i32;
            (d * d) as u32
        })
        .sum()
}

/// 智能抽帧：自适应间隔 + 场景切换/I 帧优先 + 图像指纹去重 + 帧数封顶。
///
/// 和旧版「按 index 均匀切 N 份」的区别：
/// 1. 采样点按**时间间隔**给，长视频不会因为 `video_frames` 太小而只抽到几帧；
/// 2. 间隔本身按**镜头切换密度**自适应：PPT/网课这类几乎不换镜头的视频放稀，
///    动作片/体育这类高频换镜头的视频加密（下限仍是 4 秒）；
/// 3. 每个采样窗口内优先落在**场景切换点**上，其次才是 I 帧
///    （画面最清晰、最可能是镜头切换点）；
/// 4. 抽出来的帧算 dHash，和上一张几乎一样的直接丢掉 —— 一段静止镜头
///    只会留一张，省掉大量重复的 CLIP 计算和缩略图；
/// 5. 总帧数按 `max_frames` 封顶，超出时自动放稀采样间隔。
pub fn sample_video_frames_smart(path: &Path, plan: FramePlan) -> Vec<PathBuf> {
    let Some(ff) = ffmpeg_path() else {
        return vec![];
    };
    let base = work_dir();
    let tmp = base.join(format!("frames-smart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        return vec![];
    }

    let Some(dur) = video_duration(path).filter(|duration| *duration > 0.0) else {
        return Vec::new();
    };
    // 关键帧与场景切换时间戳（探测失败就是空表，下面自然退回窗口中心点）
    let keys = video_keyframe_times(path);
    let scenes = video_scene_change_times(path, dur);

    // 兜住配置里填了 0/负数的情况；1 秒即按 1 FPS 采样。
    let interval = adaptive_interval((plan.interval as f64).max(0.1), dur, scenes.len());
    // 排障开关：设了 DEEPAISEE_DEBUG_FRAMES 就把探测结果打到 stderr。
    if std::env::var_os("DEEPAISEE_DEBUG_FRAMES").is_some() {
        eprintln!(
            "[frames] {} dur={dur:.2}s dhash_dedup={} scenes={} keys={} interval={interval:.2}s",
            path.display(),
            plan.dedup_distance,
            scenes.len(),
            keys.len()
        );
    }
    let max_frames = plan.max_frames.clamp(1, 512) as usize;
    let wanted = ((dur / interval).ceil() as usize).clamp(1, max_frames);
    // 帧数封顶后实际间隔会被拉大，避免长视频仍然按 4 秒切出几万帧。
    let step = (dur / wanted as f64).max(interval);

    // 窗口内挑一个最靠近中心的候选时间点：场景切换点 > I 帧 > 窗口中心。
    let pick_in = |candidates: &[f64], start: f64, end: f64, center: f64| -> Option<f64> {
        candidates
            .iter()
            .copied()
            .filter(|t| *t >= start && *t < end)
            .min_by(|a, b| {
                (a - center)
                    .abs()
                    .partial_cmp(&(b - center).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    };

    let mut out: Vec<PathBuf> = Vec::new();
    let mut last_sig: Option<(u64, [u8; 3])> = None;
    for i in 0..wanted {
        let start = i as f64 * step;
        let end = ((i + 1) as f64 * step).min(dur);
        let center = (start + end) / 2.0;
        let pick = pick_in(&scenes, start, end, center)
            .or_else(|| pick_in(&keys, start, end, center))
            .unwrap_or(center);
        let dest = tmp.join(format!("f{i}.jpg"));
        if !grab_frame(&ff, path, pick, plan.max_side, &dest) {
            continue;
        }
        match frame_signature(&dest) {
            Some((hash, rgb)) => {
                if let Some((prev_hash, prev_rgb)) = last_sig {
                    // 结构相近 + 颜色也相近才算「同一画面」，纯色切换不会误丢。
                    if hamming(prev_hash, hash) <= plan.dedup_distance
                        && color_distance(prev_rgb, rgb) <= 300
                    {
                        let _ = std::fs::remove_file(&dest);
                        continue;
                    }
                }
                last_sig = Some((hash, rgb));
                out.push(dest);
            }
            // 算不出指纹（解码异常）就保守保留，宁多勿漏。
            None => out.push(dest),
        }
    }
    if out.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    out
}

/// Sample a single video window at the requested 1 FPS policy. The window is
/// kept separate from the scene-aware sampler because multimodal embedding
/// needs the frame timestamps to line up with the audio segment.
pub fn sample_video_frames_window(
    path: &Path,
    start_seconds: f64,
    duration_seconds: f64,
    max_side: u32,
) -> Vec<PathBuf> {
    sample_video_frames_window_with_cancel(path, start_seconds, duration_seconds, max_side, &|| false)
}

/// Cancellation-aware variant used by the indexing worker. A stop request
/// terminates the active FFmpeg child, reaps it, and removes partial frames.
pub fn sample_video_frames_window_with_cancel<F>(
    path: &Path,
    start_seconds: f64,
    duration_seconds: f64,
    max_side: u32,
    cancelled: &F,
) -> Vec<PathBuf>
where
    F: Fn() -> bool,
{
    if cancelled() {
        return Vec::new();
    }
    let Some(ff) = ffmpeg_path() else {
        return Vec::new();
    };
    let base = work_dir();
    // Include the source path and thread identity in the work directory. The
    // indexer can process several videos concurrently; using only PID/start
    // would let two files at t=0 overwrite one another's frames.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let tmp = base.join(format!(
        "frames-window-{}-{:016x}-{}",
        std::process::id(),
        hasher.finish(),
        (start_seconds.max(0.0) * 1000.0).round() as u64
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        return Vec::new();
    }
    let count = duration_seconds.ceil().clamp(1.0, 30.0) as usize;
    let scale = format!(
        "scale='min({m},iw)':'min({m},ih)':force_original_aspect_ratio=decrease",
        m = max_side.max(64)
    );
    let output = run_video_ffmpeg_with_cancel(&ff, |cuda| {
        let mut command = Command::new(&ff);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            &format!("{start_seconds:.3}"),
        ]);
        if cuda {
            // Keep decoded frames in device memory until the explicit
            // hwdownload in the filter graph. This enables NVDEC while still
            // returning ordinary JPEG files to the embedding backends.
            command.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
        }
        command.arg("-i").arg(path);
        let filter = if cuda {
            format!("hwdownload,format=nv12,fps=1,{scale}")
        } else {
            format!("fps=1,{scale}")
        };
        command
            .args([
                "-t",
                &format!("{duration_seconds:.3}"),
                "-vf",
                &filter,
                "-q:v",
                "4",
                "-an",
            ])
            .arg(tmp.join("f-%03d.jpg"))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }, cancelled);
    if !cancelled() && output.map(|value| value.status.success()).unwrap_or(false) {
        let mut frames: Vec<PathBuf> = std::fs::read_dir(&tmp)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        frames.sort();
        frames.truncate(count);
        if !frames.is_empty() {
            return frames;
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Vec::new()
}

/// Decode one bounded video window while overlapping the independent audio
/// and frame paths. Only one short-lived audio worker is created per window;
/// callers still advance windows serially (or through their own bounded
/// worker pool), so this does not create an unbounded FFmpeg process storm.
/// The returned frame paths have the same ownership/lifetime as
/// `sample_video_frames_window` and must be removed by the caller after image
/// decoding.
pub fn decode_video_window(
    path: &Path,
    start_seconds: f64,
    duration_seconds: f64,
    max_side: u32,
    include_video: bool,
    include_audio: bool,
    audio_sample_rate: u32,
) -> (Vec<PathBuf>, Option<Vec<f32>>) {
    decode_video_window_with_cancel(
        path,
        start_seconds,
        duration_seconds,
        max_side,
        include_video,
        include_audio,
        audio_sample_rate,
        &|| false,
    )
}

/// Cancellation-aware version of [`decode_video_window`]. Both FFmpeg paths
/// share the same callback, and the scoped audio worker is joined before this
/// function returns so callers can safely checkpoint or unload a model.
pub fn decode_video_window_with_cancel<F>(
    path: &Path,
    start_seconds: f64,
    duration_seconds: f64,
    max_side: u32,
    include_video: bool,
    include_audio: bool,
    audio_sample_rate: u32,
    cancelled: &F,
) -> (Vec<PathBuf>, Option<Vec<f32>>)
where
    F: Fn() -> bool + Sync,
{
    std::thread::scope(|scope| {
        let audio_job = if include_audio {
            let audio_path = path.to_path_buf();
            Some(scope.spawn(move || {
                load_audio_segment_at_rate_with_cancel(
                    &audio_path,
                    start_seconds,
                    duration_seconds,
                    audio_sample_rate,
                    cancelled,
                )
            }))
        } else {
            None
        };
        let frames = if include_video && !cancelled() {
            sample_video_frames_window_with_cancel(
                path,
                start_seconds,
                duration_seconds,
                max_side,
                cancelled,
            )
        } else {
            Vec::new()
        };
        let audio = audio_job.and_then(|job| job.join().ok().flatten());
        (frames, audio)
    })
}

/// Pick a video window that fits the model's shared multimodal context.
///
/// EmbeddingGemma2 accounts for roughly one soft token per configured video
/// frame budget and about 25 audio tokens per second.  The native GGUF path
/// enforces this limit before it submits the sequence, so a fixed 30-second
/// window can fail on models configured with a 4096-token context.  Keep a
/// small margin for modality markers and text tokens, then let callers advance
/// by the returned duration so every frame/audio sample is still indexed.
pub fn safe_video_window_seconds(
    context_tokens: usize,
    frame_token_budget: usize,
    include_audio: bool,
    audio_seconds: f32,
) -> f64 {
    // Use the requested 30-second media window whenever the model context can
    // hold it. Smaller context configurations are reduced only as far as the
    // token budget requires, so a 4K model remains safe while 8K+ models get
    // the intended 30-second temporal coverage.
    const MAX_WINDOW_SECONDS: f64 = 30.0;
    const AUDIO_TOKENS_PER_SECOND: usize = 25;
    const CONTEXT_MARGIN: usize = 256;

    let context = context_tokens.max(512);
    let frame_cost = frame_token_budget.clamp(70, 1120);
    let per_second = frame_cost.saturating_add(if include_audio {
        AUDIO_TOKENS_PER_SECOND
    } else {
        0
    });
    let available = context.saturating_sub(CONTEXT_MARGIN);
    let by_budget = if per_second == 0 {
        MAX_WINDOW_SECONDS as usize
    } else {
        available / per_second
    };
    // `audio_seconds` is an audio sampling preference. It must not shorten
    // video-only windows when the audio tower is disabled; doing so creates
    // needless extra FFmpeg processes and model calls for frame-only indexing.
    let configured_audio = if include_audio {
        f64::from(audio_seconds).clamp(0.25, MAX_WINDOW_SECONDS)
    } else {
        MAX_WINDOW_SECONDS
    };
    (by_budget.max(1) as f64)
        .min(MAX_WINDOW_SECONDS)
        .min(configured_audio)
}
/// 清掉上次运行残留的 `frames-<pid>` 目录。
///
/// 每个进程只用自己的 pid 目录，正常结束会删；但程序被强杀 / 崩溃时
/// 目录会留下来，日积月累能堆出几百兆的 jpg。启动新一轮抽帧时顺手清掉
/// 那些「不是当前进程」的旧目录（失败也无所谓，下次再清）。
// Window-specific frame directories are removed after decoding, so no global
// sweep is needed and concurrent video jobs cannot delete one another's data.

fn parse_ffmpeg_duration(text: &str) -> Option<f64> {
    let idx = text.find("Duration: ")?;
    let rest = &text[idx + "Duration: ".len()..];
    let end = rest.find(',')?;
    let parts: Vec<&str> = rest[..end].split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].trim().parse().ok()?;
    let m: f64 = parts[1].trim().parse().ok()?;
    let s: f64 = parts[2].trim().parse().ok()?;
    let duration = h * 3600.0 + m * 60.0 + s;
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

fn parse_ffprobe_duration(json: &str) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let format_duration = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|duration| {
            duration
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| duration.as_f64())
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0);
    let stream_duration = value
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|stream| stream.get("duration"))
        .filter_map(|duration| {
            duration
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| duration.as_f64())
        })
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .fold(0.0f64, f64::max);
    format_duration.or_else(|| (stream_duration > 0.0).then_some(stream_duration))
}

/// Video duration in seconds. Prefer machine-readable ffprobe metadata because
/// FFmpeg's human-readable `Duration:` line is absent for some fragmented and
/// streaming containers. The stderr parser remains a fallback for minimal installs.
pub fn media_duration(path: &Path) -> Option<f64> {
    let ff = ffmpeg_path()?;
    if let Some(probe) = ffprobe_path(&ff) {
        if let Ok(output) = hidden(
            Command::new(probe)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration:stream=duration",
                    "-of",
                    "json",
                ])
                .arg(path)
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .output()
        {
            if output.status.success() {
                if let Some(duration) =
                    parse_ffprobe_duration(&String::from_utf8_lossy(&output.stdout))
                {
                    return Some(duration);
                }
            }
        }
    }

    // FFmpeg reports container duration while probing inputs, even though
    // this command intentionally has no output target and exits with an error.
    let out = hidden(
        Command::new(&ff)
            .args(["-hide_banner", "-i"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
    .output()
    .ok()?;
    parse_ffmpeg_duration(&String::from_utf8_lossy(&out.stderr))
}

pub fn video_duration(path: &Path) -> Option<f64> {
    media_duration(path)
}

/// Require a valid media duration before creating temporally addressable
/// video vectors. Guessing a short duration here would silently index only
/// the opening frames while claiming the whole video was processed.
pub fn require_video_duration(path: &Path) -> Result<f64, String> {
    video_duration(path)
        .filter(|duration| duration.is_finite() && *duration > 0.0)
        .ok_or_else(|| {
            format!(
                "无法读取视频时长，未建立视频分段索引：{}（请检查 FFmpeg/ffprobe 和媒体文件）",
                path.display()
            )
        })
}

/// 把音频转成 16k 单声道 f32 采样。
pub fn load_audio(path: &Path, seconds: f64) -> Option<Vec<f32>> {
    load_audio_at_rate(path, seconds, 16_000)
}

/// Decode mono little-endian PCM at the sample rate declared by the active
/// multimodal projector. EmbeddingGemma2 uses 16 kHz; other mtmd projectors
/// may declare a different rate.
pub fn load_audio_at_rate(path: &Path, seconds: f64, sample_rate: u32) -> Option<Vec<f32>> {
    let ff = ffmpeg_path()?;
    let sample_rate = sample_rate.clamp(8_000, 192_000);
    let out = hidden(
        Command::new(&ff)
            .arg("-v")
            .arg("quiet")
            .args(["-i"])
            .arg(path)
            .args([
                "-t",
                &format!("{seconds:.2}"),
                "-ac",
                "1",
                "-ar",
                &sample_rate.to_string(),
                "-f",
                "f32le",
                "-",
            ]),
    )
    .output()
    .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let bytes = out.stdout;
    let n = bytes.len() / 4;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let b = [
            bytes[i * 4],
            bytes[i * 4 + 1],
            bytes[i * 4 + 2],
            bytes[i * 4 + 3],
        ];
        v.push(f32::from_le_bytes(b));
    }
    Some(v)
}

/// Decode the audio track aligned to a video window. EmbeddingGemma2 expects
/// mono 16 kHz PCM, and the model itself caps a clip at 30 seconds.
pub fn load_audio_segment(path: &Path, start_seconds: f64, seconds: f64) -> Option<Vec<f32>> {
    load_audio_segment_at_rate(path, start_seconds, seconds, 16_000)
}

/// Decode one video-window audio track at the projector's required rate.
pub fn load_audio_segment_at_rate(
    path: &Path,
    start_seconds: f64,
    seconds: f64,
    sample_rate: u32,
) -> Option<Vec<f32>> {
    load_audio_segment_at_rate_with_cancel(path, start_seconds, seconds, sample_rate, &|| false)
}

/// Cancellation-aware variant for one short video audio window. The child
/// process is polled while its PCM is drained, so cancellation cannot leave
/// a decoder running after the indexer has moved to its durable stop point.
pub fn load_audio_segment_at_rate_with_cancel<F>(
    path: &Path,
    start_seconds: f64,
    seconds: f64,
    sample_rate: u32,
    cancelled: &F,
) -> Option<Vec<f32>>
where
    F: Fn() -> bool,
{
    if cancelled() {
        return None;
    }
    let ff = ffmpeg_path()?;
    let duration = seconds.clamp(0.25, 30.0);
    let sample_rate = sample_rate.clamp(8_000, 192_000);
    let mut command = Command::new(&ff);
    let command = hidden(
        command
            .arg("-v")
            .arg("quiet")
            .arg("-ss")
            .arg(format!("{start_seconds:.3}"))
            .arg("-threads")
            .arg("0")
            .arg("-i")
            .arg(path)
            .args([
                "-t",
                &format!("{duration:.2}"),
                "-map",
                "0:a:0?",
                "-vn",
                "-ac",
                "1",
                "-ar",
                &sample_rate.to_string(),
                "-f",
                "f32le",
                "-",
            ]),
    );
    let out = wait_child_with_cancel(command.spawn().ok()?, cancelled).ok()?;
    if cancelled() || !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let bytes = out.stdout;
    let mut samples = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(samples)
}

/// 把音频转成 48k 单声道 f32 采样（CLAP 的输入采样率）。
///
/// 不能复用 `load_audio` 的 16k：CLAP 的 64 个 mel 滤波器上限到 14kHz，
/// 采样率降一半，高频整段消失，海浪/鸟鸣这类声音就分不出来了。
pub fn load_audio_48k(path: &Path, seconds: f32) -> Option<Vec<f32>> {
    let ff = ffmpeg_path()?;
    let mut command = Command::new(&ff);
    let out = hidden(
        command
            .arg("-v")
            .arg("quiet")
            .args(["-i"])
            .arg(path)
            .args([
                "-t",
                &format!("{seconds:.2}"),
                "-ac",
                "1",
                "-ar",
                "48000",
                "-f",
                "f32le",
                "-",
            ]),
    )
    .output()
    .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let bytes = out.stdout;
    let n = bytes.len() / 4;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let b = [
            bytes[i * 4],
            bytes[i * 4 + 1],
            bytes[i * 4 + 2],
            bytes[i * 4 + 3],
        ];
        v.push(f32::from_le_bytes(b));
    }
    Some(v)
}

/// 生成缩略图（jpeg），返回是否成功。
pub fn make_thumb(src: &Path, dest: &Path, size: u32) -> bool {
    let is_video = matches!(
        src.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .as_deref(),
        Some("mp4")
            | Some("mkv")
            | Some("avi")
            | Some("mov")
            | Some("wmv")
            | Some("webm")
            | Some("flv")
            | Some("m4v")
    );

    // 图片直接用 image crate 解码缩放，不必依赖 ffmpeg（也快得多）
    if !is_video {
        if let Ok(img) = image::open(src) {
            let img = img.thumbnail(size, size);
            if img.save_with_format(dest, image::ImageFormat::Jpeg).is_ok() {
                return dest.is_file();
            }
        }
        // 遇到 image crate 不认识的格式（heic 等）再退回 ffmpeg
    }

    let Some(ff) = ffmpeg_path() else {
        return false;
    };
    let mut cmd = Command::new(&ff);
    cmd.arg("-y");
    if is_video {
        // 取中间帧
        let dur = video_duration(src).unwrap_or(2.0);
        cmd.args(["-ss", &format!("{:.3}", dur / 2.0)]);
    }
    cmd.arg("-i").arg(src).args(["-frames:v", "1"]);
    let scale = format!(
        "scale='min({s},iw)':'min({s},ih)':force_original_aspect_ratio=decrease",
        s = size
    );
    cmd.args(["-vf", &scale, "-q:v", "4"]).arg(dest);
    hidden(&mut cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success() && dest.is_file())
        .unwrap_or(false)
}

/// Generate document thumbnails when a document contains a usable image.
/// PDFs are rendered by PDF.js in the gallery so scanned pages and vector
/// covers are represented correctly; DOCX stores previewable images under
/// `word/media`.
pub fn make_document_thumb(src: &Path, dest: &Path, size: u32) -> bool {
    let ext = src
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let image = if ext == "pdf" {
        // PDF thumbnails must show a rendered page, not whichever embedded
        // image object happens to appear first. The gallery renders page 1
        // through PDF.js and persists that cover in the thumbnail cache.
        None
    } else if ext == "docx" {
        let Some(file) = std::fs::File::open(src).ok() else {
            return false;
        };
        let Ok(mut archive) = zip::ZipArchive::new(file) else {
            return false;
        };
        let mut names: Vec<String> = archive
            .file_names()
            .filter(|name| name.starts_with("word/media/"))
            .map(str::to_owned)
            .collect();
        names.sort();
        let mut image = None;
        for name in names {
            let Ok(mut entry) = archive.by_name(&name) else {
                continue;
            };
            let mut bytes = Vec::new();
            if std::io::Read::read_to_end(&mut entry, &mut bytes).is_err() {
                continue;
            }
            if let Ok(value) = image::load_from_memory(&bytes) {
                image = Some(value);
                break;
            }
        }
        image
    } else {
        None
    };
    let Some(image) = image else { return false };
    save_document_thumb_image(&image, dest, size)
}

/// Save an already decoded document page as a gallery thumbnail.
pub fn save_document_thumb_image(image: &image::DynamicImage, dest: &Path, size: u32) -> bool {
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    let image = image.thumbnail(size, size);
    image
        .save_with_format(dest, image::ImageFormat::Jpeg)
        .is_ok()
        && dest.is_file()
}

/// 视频基本信息（宽高）。
pub fn video_size(path: &Path) -> Option<(u32, u32)> {
    let ff = ffmpeg_path()?;
    let out = hidden(
        Command::new(&ff)
            .arg("-i")
            .arg(path)
            .args(["-f", "null", "-"]),
    )
    .output()
    .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    // Video: h264, yuv420p, 320x240
    for line in text.lines() {
        if let Some(pos) = line.find("Video:") {
            let rest = &line[pos..];
            if let Some(x) = rest.find('x') {
                let before = &rest[..x];
                let after = &rest[x + 1..];
                let w: String = before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                let h: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                    return Some((w, h));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        decode_video_window_with_cancel, parse_ffmpeg_duration, parse_ffmpeg_subtitle_streams,
        parse_ffprobe_duration, parse_subtitle_text, safe_video_window_seconds,
        subtitle_cues_for_window, subtitle_text_for_window,
        subtitle_text_for_window_with_timestamps, TimedSubtitle,
    };

    #[test]
    fn video_windows_fit_common_context_sizes() {
        assert_eq!(safe_video_window_seconds(4096, 140, true, 30.0), 23.0);
        assert_eq!(safe_video_window_seconds(8192, 140, true, 30.0), 30.0);
        assert_eq!(safe_video_window_seconds(4096, 1120, true, 30.0), 3.0);
        assert_eq!(safe_video_window_seconds(4096, 140, false, 30.0), 27.0);
        // Audio preferences do not fragment a video-only pass.
        assert_eq!(safe_video_window_seconds(8192, 140, false, 2.0), 30.0);
    }

    #[test]
    fn video_window_never_returns_zero() {
        assert_eq!(safe_video_window_seconds(256, 1120, true, 30.0), 1.0);
        assert_eq!(safe_video_window_seconds(4096, 140, true, 2.0), 2.0);
    }

    #[test]
    fn cancelled_video_window_returns_without_starting_media_decoders() {
        let (frames, audio) = decode_video_window_with_cancel(
            std::path::Path::new("missing-video-for-cancellation-test.mp4"),
            0.0,
            1.0,
            256,
            true,
            true,
            16_000,
            &|| true,
        );
        assert!(frames.is_empty());
        assert!(audio.is_none());
    }

    #[test]
    fn parses_srt_and_vtt_cues_and_maps_overlapping_windows() {
        let srt = "1\n00:00:28,500 --> 00:00:31,200\nEngine starts <i>now</i>\n\n2\n00:01:02,000 --> 00:01:03,000\nTurn left\n";
        let cues = parse_subtitle_text(srt);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_seconds, 28.5);
        assert_eq!(cues[0].text, "Engine starts now");
        assert_eq!(
            subtitle_text_for_window(&cues, 30.0, 60.0),
            "Engine starts now"
        );
        assert_eq!(subtitle_text_for_window(&cues, 60.0, 90.0), "Turn left");
        assert_eq!(subtitle_cues_for_window(&cues, 30.0, 60.0).len(), 1);
        assert_eq!(
            subtitle_text_for_window_with_timestamps(&cues, 30.0, 60.0),
            "[00:28.500-00:31.200] Engine starts now"
        );

        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.500 align:start\nhello\n";
        let vtt_cues = parse_subtitle_text(vtt);
        assert_eq!(vtt_cues.len(), 1);
        assert_eq!(vtt_cues[0].end_seconds, 2.5);
    }

    #[test]
    fn parses_ass_dialogue_and_ffmpeg_subtitle_streams() {
        let ass = "[Events]\nDialogue: 0,0:00:04.00,0:00:05.50,Default,,0,0,0,,Hello\\Nworld\n";
        let cues = parse_subtitle_text(ass);
        assert_eq!(
            cues,
            vec![TimedSubtitle {
                start_seconds: 4.0,
                end_seconds: 5.5,
                text: "Hello world".into()
            }]
        );

        let ffmpeg =
            "  Stream #0:2(eng): Subtitle: subrip\n  Stream #0:4: Subtitle: hdmv_pgs_subtitle";
        assert_eq!(
            parse_ffmpeg_subtitle_streams(ffmpeg),
            vec![(2, "subrip".into()), (4, "hdmv_pgs_subtitle".into())]
        );
    }

    #[test]
    fn parses_ffprobe_duration_and_falls_back_to_valid_streams() {
        assert_eq!(
            parse_ffprobe_duration(
                r#"{"format":{"duration":"120.5"},"streams":[{"duration":"121"}]}"#
            ),
            Some(120.5)
        );
        assert_eq!(
            parse_ffprobe_duration(
                r#"{"format":{"duration":"N/A"},"streams":[{"duration":"15.25"},{"duration":"17"}]}"#
            ),
            Some(17.0)
        );
        assert_eq!(
            parse_ffprobe_duration(r#"{"format":{"duration":"0"},"streams":[{"duration":"9.5"}]}"#),
            Some(9.5)
        );
        assert_eq!(
            parse_ffprobe_duration(
                r#"{"format":{"duration":"NaN"},"streams":[{"duration":"-1"},{"duration":"Infinity"}]}"#
            ),
            None
        );
    }

    #[test]
    fn parses_ffmpeg_duration_fallback() {
        assert_eq!(
            parse_ffmpeg_duration("Duration: 01:02:03.50, start: 0.000000, bitrate: 1000 kb/s"),
            Some(3723.5)
        );
        assert_eq!(
            parse_ffmpeg_duration("Duration: N/A, start: 0.000000"),
            None
        );
    }
}
