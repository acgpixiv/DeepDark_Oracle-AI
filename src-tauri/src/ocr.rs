//! Optional TeleOCR sidecar. The worker is lazy-started and keeps one model
//! loaded while the application is running.
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fs, time};
use tauri::State;

const WORKER_SOURCE: &str = include_str!("../../scripts/teleocr_worker.py");
const OCR_STATUS_CACHE_TTL: Duration = Duration::from_secs(20);

struct OcrStatusCache {
    key: String,
    created: Instant,
    value: OcrStatus,
}

static OCR_STATUS_CACHE: OnceLock<Mutex<Option<OcrStatusCache>>> = OnceLock::new();

/// Start helper processes without creating a transient console window on
/// Windows. Model settings repeatedly probe Python, Tesseract and the PDF
/// renderer; those probes must remain invisible to the user.
fn hidden_command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

#[derive(Debug, Clone, Default)]
pub struct OcrConfig {
    pub model_dir: String,
    pub python_path: String,
    pub code_dir: String,
}

impl From<&crate::config::ModelConfig> for OcrConfig {
    fn from(config: &crate::config::ModelConfig) -> Self {
        Self {
            model_dir: config.teleocr_model_dir.clone(),
            python_path: config.teleocr_python_path.clone(),
            code_dir: config.teleocr_code_dir.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OcrStatus {
    pub python_available: bool,
    pub teleocr_available: bool,
    pub model_configured: bool,
    /// Tesseract is the lightweight fallback when no Python OCR model is ready.
    pub tesseract_available: bool,
    pub pdf_renderer_available: bool,
    /// Names of likely OCR model directories found in the local model store.
    pub model_candidates: Vec<String>,
    /// Missing runtime pieces for a configured model (for example `torch`).
    pub missing_dependencies: Vec<String>,
    pub python_path: String,
    pub code_dir: String,
    pub model_dir: String,
    pub detail: String,
}

#[derive(Deserialize)]
struct ProbeResult {
    ok: bool,
    detail: String,
}

#[derive(Deserialize)]
struct WorkerPage {
    page: usize,
    text: String,
}

#[derive(Deserialize)]
struct WorkerResponse {
    ok: bool,
    #[serde(default)]
    error: String,
    #[serde(default)]
    pages: Vec<WorkerPage>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    trace: String,
}

struct Worker {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
    output_thread: Option<JoinHandle<()>>,
    python: PathBuf,
    model_dir: PathBuf,
    code_dir: PathBuf,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.output_thread.take() {
            let _ = reader.join();
        }
    }
}

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Default)]
pub struct OcrPdfResult {
    pub pages: Vec<(usize, String)>,
    pub errors: Vec<String>,
}

impl Worker {
    fn receive(&mut self, cancelled: &dyn Fn() -> bool) -> Result<WorkerResponse, String> {
        let started = Instant::now();
        loop {
            if cancelled() {
                return Err("index cancellation requested".into());
            }
            if started.elapsed() >= RESPONSE_TIMEOUT {
                return Err("TeleOCR worker response timed out after 10 minutes".into());
            }
            match self.output.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    return serde_json::from_str(&line).map_err(|error| {
                        format!("invalid TeleOCR worker response: {error}; output: {}", line.trim())
                    });
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("TeleOCR worker exited without a response".into());
                }
            }
        }
    }

    fn send(&mut self, request: &serde_json::Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, request)
            .map_err(|error| format!("write TeleOCR request: {error}"))?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|error| format!("flush TeleOCR request: {error}"))
    }
}

fn worker_error(prefix: &str, response: &WorkerResponse) -> String {
    let mut message = if response.error.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", response.error)
    };
    if !response.trace.trim().is_empty() {
        message.push('\n');
        message.push_str(response.trace.trim());
    }
    message
}

fn start_worker(
    python: &Path,
    model_dir: &Path,
    code_dir: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Worker, String> {
    let mut child = hidden_command(python)
        .args(["-u", "-c", WORKER_SOURCE])
        .arg(model_dir)
        .arg(code_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start TeleOCR worker: {error}"))?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| "cannot connect to TeleOCR worker stdin".to_string())?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| "cannot connect to TeleOCR worker stdout".to_string())?;
    let (sender, receiver) = mpsc::channel();
    let output_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if sender.send(line).is_err() => break,
                Ok(_) => {}
            }
        }
    });
    let mut worker = Worker {
        child,
        input,
        output: receiver,
        output_thread: Some(output_thread),
        python: normalized(python),
        model_dir: normalized(model_dir),
        code_dir: normalized(code_dir),
    };
    let ready = worker.receive(cancelled)?;
    if !ready.ok {
        return Err(worker_error("TeleOCR model failed to load", &ready));
    }
    Ok(worker)
}

fn ensure_worker(
    slot: &mut Option<Worker>,
    python: &Path,
    model_dir: &Path,
    code_dir: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let restart = slot.as_mut().map_or(true, |worker| {
        worker.python != normalized(python)
            || worker.model_dir != normalized(model_dir)
            || worker.code_dir != normalized(code_dir)
            || worker.child.try_wait().ok().flatten().is_some()
    });
    if restart {
        *slot = None;
        *slot = Some(start_worker(python, model_dir, code_dir, cancelled)?);
    }
    Ok(())
}

static WORKER: OnceLock<Mutex<Option<Worker>>> = OnceLock::new();

fn worker_slot() -> &'static Mutex<Option<Worker>> {
    WORKER.get_or_init(|| Mutex::new(None))
}

fn clean_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn python_path(configured: &str) -> PathBuf {
    if let Some(path) = clean_path(configured) {
        return path;
    }
    for root in local_roots() {
        let candidate = root.join(".tools").join("python").join("python.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("python")
}

fn local_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut path = exe.parent().map(Path::to_path_buf);
        for _ in 0..4 {
            let Some(dir) = path else { break };
            roots.push(dir.clone());
            path = dir.parent().map(Path::to_path_buf);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn find_program(name: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate.clone());
        }
    }
    let probe = if cfg!(windows) { "--version" } else { "--version" };
    hidden_command(name)
        .arg(probe)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| PathBuf::from(name))
}

fn tesseract_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for root in local_roots() {
        candidates.push(root.join(".tools").join("tesseract").join("tesseract.exe"));
        candidates.push(root.join("tools").join("tesseract").join("tesseract.exe"));
    }
    candidates.extend([
        PathBuf::from(r"C:\Program Files\Tesseract-OCR\tesseract.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Tesseract-OCR\tesseract.exe"),
    ]);
    find_program("tesseract", &candidates)
}

fn pdf_renderer_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for root in local_roots() {
        candidates.push(root.join(".tools").join("poppler").join("Library").join("bin").join("pdftoppm.exe"));
        candidates.push(root.join(".tools").join("poppler").join("bin").join("pdftoppm.exe"));
        candidates.push(root.join(".tools").join("mupdf").join("mutool.exe"));
    }
    find_program("pdftoppm", &candidates).or_else(|| find_program("mutool", &candidates))
}

/// Render one PDF page without requiring an OCR engine. This is used as a
/// visual-index fallback for vector-only pages and embedded image formats
/// that the lightweight lopdf raster decoder cannot reconstruct.
pub fn render_pdf_page_image<F>(pdf_path: &Path, page: usize, cancelled: F) -> Result<image::DynamicImage, String>
where
    F: Fn() -> bool,
{
    if page == 0 {
        return Err("page number must be one-based".into());
    }
    let renderer = pdf_renderer_path()
        .ok_or_else(|| "未找到 PDF 渲染器（Poppler pdftoppm 或 MuPDF mutool）".to_string())?;
    let is_pdftoppm = renderer
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("pdftoppm.exe") || name.eq_ignore_ascii_case("pdftoppm"));
    let stamp = time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("deepaisee-pdf-image-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&root).map_err(|error| format!("创建 PDF 图像临时目录失败：{error}"))?;
    let output_path = root.join("page.png");
    let mut command = hidden_command(&renderer);
    if is_pdftoppm {
        command
            .args(["-f", &page.to_string(), "-l", &page.to_string(), "-png", "-r", "110", "-singlefile"])
            .arg(pdf_path)
            .arg(root.join("page"));
    } else {
        command
            .args(["draw", "-F", "png", "-r", "110", "-o"])
            .arg(&output_path)
            .arg(pdf_path)
            .arg(page.to_string());
    }
    let output = run_cancellable(&mut command, &cancelled);
    let result = match output {
        Ok(output) if output.status.success() && output_path.is_file() => image::open(&output_path)
            .map(|image| image.thumbnail(1024, 1024).to_rgb8().into())
            .map_err(|error| format!("读取 PDF 第 {page} 页渲染图失败：{error}")),
        Ok(output) if !output.status.success() => Err(format!(
            "PDF 第 {page} 页渲染失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(_) => Err(format!("PDF 第 {page} 页渲染器没有生成图像")),
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(root);
    result
}

fn tesseract_languages(exe: &Path) -> String {
    let output = hidden_command(exe).arg("--list-langs").output();
    let text = output
        .ok()
        .map(|value| format!("{}\n{}", String::from_utf8_lossy(&value.stdout), String::from_utf8_lossy(&value.stderr)))
        .unwrap_or_default();
    let has_chinese = text.lines().any(|line| line.trim().eq_ignore_ascii_case("chi_sim"));
    let has_english = text.lines().any(|line| line.trim().eq_ignore_ascii_case("eng"));
    match (has_chinese, has_english) {
        (true, true) => "chi_sim+eng".to_string(),
        (true, false) => "chi_sim".to_string(),
        (_, true) => "eng".to_string(),
        _ => "eng".to_string(),
    }
}

fn tesseract_status() -> (bool, bool, String) {
    let tesseract = tesseract_path();
    let renderer = pdf_renderer_path();
    match (tesseract, renderer) {
        (Some(exe), Some(renderer)) => (
            true,
            true,
            format!("Tesseract OCR available ({}, PDF renderer: {})", tesseract_languages(&exe), renderer.display()),
        ),
        (Some(exe), None) => (true, false, format!("Tesseract available ({}) but no PDF renderer (pdftoppm/mutool)", tesseract_languages(&exe))),
        (None, _) => (false, false, "Tesseract OCR not found; install Tesseract or place it in .tools/tesseract".into()),
    }
}

/// Run a subprocess while polling the indexing cancellation callback. Pipes
/// are drained on dedicated threads so a noisy renderer cannot block on a
/// full stdout/stderr pipe while the caller waits for it.
fn run_cancellable(command: &mut Command, cancelled: &dyn Fn() -> bool) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start subprocess: {error}"))?;
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });

    let status = loop {
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = stdout { let _ = reader.join(); }
            if let Some(reader) = stderr { let _ = reader.join(); }
            return Err("index cancellation requested".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(reader) = stdout { let _ = reader.join(); }
                if let Some(reader) = stderr { let _ = reader.join(); }
                return Err(format!("failed waiting for subprocess: {error}"));
            }
        }
    };
    let stdout = stdout.and_then(|reader| reader.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|reader| reader.join().ok()).unwrap_or_default();
    Ok(Output { status, stdout, stderr })
}

/// Returns true when a user-installed Tesseract executable is available.
/// The application deliberately does not install the system package silently:
/// the official Windows distribution is an installer and may modify PATH or
/// the registry. Users can also place a portable executable under .tools.
pub fn tesseract_available() -> bool {
    tesseract_path().is_some()
}

pub fn pdf_ocr_available() -> bool {
    tesseract_path().is_some() && pdf_renderer_path().is_some()
}

fn tesseract_text_default_with_cancel(image: &Path, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
    let executable = tesseract_path().ok_or_else(|| {
        "Tesseract OCR 未找到；请安装 Tesseract 或将 tesseract.exe 放入项目 .tools/tesseract".to_string()
    })?;
    let languages = tesseract_languages(&executable);
    let mut command = hidden_command(&executable);
    command
        .arg(image)
        .arg("stdout")
        .arg("-l")
        .arg(languages)
        .arg("--psm")
        .arg("3");
    let output = run_cancellable(&mut command, cancelled).map_err(|error| {
        if error == "index cancellation requested" {
            error
        } else {
            format!("启动 Tesseract 失败：{error}")
        }
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("Tesseract OCR 退出码 {:?}", output.status.code())
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// OCR a standalone image with Tesseract. This is intentionally independent
/// from the optional Python worker so the default path remains lightweight.
pub fn extract_image_text_tesseract(image_path: &Path) -> Result<String, String> {
    extract_image_text_tesseract_with_cancel(image_path, &|| false)
}

pub fn extract_image_text_tesseract_with_cancel(
    image_path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<String, String> {
    if !image_path.is_file() {
        return Err(format!("图片文件不存在：{}", image_path.display()));
    }
    tesseract_text_default_with_cancel(image_path, cancelled)
}

fn temporary_ocr_stem(_pdf_path: &Path, page: usize) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "deepaisee-ocr-{}-{}-{}",
        std::process::id(),
        stamp,
        page
    ))
}

/// Render selected PDF pages with the locally available Poppler/MuPDF binary
/// and run Tesseract on each rendered page. Rendering is kept page-by-page so
/// cancellation can stop a large scanned document promptly.
pub fn extract_pdf_pages_tesseract<F>(
    pdf_path: &Path,
    pages_1_based: &[usize],
    cancelled: F,
) -> Result<OcrPdfResult, String>
where
    F: Fn() -> bool,
{
    if !pdf_path.is_file() {
        return Err(format!("PDF 文件不存在：{}", pdf_path.display()));
    }
    let renderer = pdf_renderer_path().ok_or_else(|| {
        "未找到 PDF 渲染器；请安装 Poppler(pdftoppm)或 MuPDF(mutool)后再使用扫描件 OCR".to_string()
    })?;
    if tesseract_path().is_none() {
        return Err("Tesseract OCR 未找到；请安装 Tesseract 或将 tesseract.exe 放入项目 .tools/tesseract".into());
    }
    let is_mutool = renderer
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("mutool.exe") || name.eq_ignore_ascii_case("mutool"));
    let mut result = OcrPdfResult::default();
    for &page in pages_1_based {
        if cancelled() {
            return Err("index cancellation requested".into());
        }
        if page == 0 {
            result.errors.push("page number must be one-based".into());
            continue;
        }
        let stem = temporary_ocr_stem(pdf_path, page);
        let render_output = if is_mutool {
            let pattern = format!("{}-%d.png", stem.display());
            let mut command = hidden_command(&renderer);
            command.args(["draw", "-F", "png", "-r", "180", "-o"])
                .arg(pattern)
                .arg(pdf_path)
                .arg(page.to_string());
            run_cancellable(&mut command, &cancelled)
        } else {
            let mut command = hidden_command(&renderer);
            command.args(["-f"])
                .arg(page.to_string())
                .args(["-l"])
                .arg(page.to_string())
                .args(["-png", "-r", "180", "-singlefile"])
                .arg(pdf_path)
                .arg(&stem);
            run_cancellable(&mut command, &cancelled)
        };
        let render_output = match render_output {
            Ok(output) => output,
            Err(error) => {
                if error == "index cancellation requested" {
                    let _ = std::fs::remove_file(stem.with_extension("png"));
                    return Err(error);
                }
                result.errors.push(format!("page {page}: PDF 渲染失败：{error}"));
                continue;
            }
        };
        let image = if is_mutool {
            PathBuf::from(format!("{}-{}.png", stem.display(), page))
        } else {
            stem.with_extension("png")
        };
        if !render_output.status.success() || !image.is_file() {
            let detail = String::from_utf8_lossy(&render_output.stderr).trim().to_string();
            result.errors.push(format!(
                "page {page}: PDF 渲染失败{}",
                if detail.is_empty() { String::new() } else { format!("：{detail}") }
            ));
            let _ = std::fs::remove_file(&image);
            continue;
        }
        match tesseract_text_default_with_cancel(&image, &cancelled) {
            Ok(text) => result.pages.push((page, text)),
            Err(error) if error == "index cancellation requested" => {
                let _ = std::fs::remove_file(&image);
                return Err(error);
            }
            Err(error) => result.errors.push(format!("page {page}: {error}")),
        }
        let _ = std::fs::remove_file(&image);
    }
    Ok(result)
}

/// Select the OCR engine once per indexing batch. A configured TeleOCR model
/// wins; otherwise the lightweight Tesseract fallback is used when present.
pub fn teleocr_ready(config: &OcrConfig) -> bool {
    is_configured(&config.model_dir, Some(&config.python_path), Some(&config.code_dir))
}

fn tesseract_text(exe: &Path, image_path: &Path, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
    let languages = tesseract_languages(exe);
    let mut command = hidden_command(exe);
    command
        .arg(image_path)
        .arg("stdout")
        .args(["-l", languages.as_str(), "--psm", "6"]);
    let output = run_cancellable(&mut command, cancelled).map_err(|error| {
        if error == "index cancellation requested" {
            error
        } else {
            format!("无法启动 Tesseract：{error}")
        }
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("Tesseract OCR 失败（退出码 {:?}）", output.status.code())
        } else {
            detail
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn tesseract_image(image_path: &Path, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
    if cancelled() {
        return Err("index cancellation requested".into());
    }
    let exe = tesseract_path().ok_or_else(|| "未找到 Tesseract OCR，请安装 Tesseract 或放入 .tools/tesseract".to_string())?;
    tesseract_text(&exe, image_path, cancelled)
}

fn tesseract_pdf_pages(
    pdf_path: &Path,
    pages: &[usize],
    cancelled: &dyn Fn() -> bool,
) -> Result<OcrPdfResult, String> {
    let exe = tesseract_path().ok_or_else(|| "未找到 Tesseract OCR，请安装 Tesseract 或放入 .tools/tesseract".to_string())?;
    let renderer = pdf_renderer_path().ok_or_else(|| "未找到 PDF 渲染器，请安装 Poppler（pdftoppm）或 MuPDF（mutool）".to_string())?;
    let root = std::env::temp_dir().join(format!("deepdark-ocr-{}-{}", std::process::id(), time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
    fs::create_dir_all(&root).map_err(|error| format!("创建 OCR 临时目录失败：{error}"))?;
    let prefix = root.join("page");
    let mut result = OcrPdfResult::default();
    let is_pdftoppm = renderer.file_name().and_then(|v| v.to_str()).map(|v| v.eq_ignore_ascii_case("pdftoppm.exe") || v.eq_ignore_ascii_case("pdftoppm")).unwrap_or(false);
    for &page in pages {
        if cancelled() {
            let _ = fs::remove_dir_all(&root);
            return Err("index cancellation requested".into());
        }
        let image_path = if is_pdftoppm {
            let mut command = hidden_command(&renderer);
            command.args(["-f", &page.to_string(), "-l", &page.to_string(), "-png", "-r", "180"])
                .arg(pdf_path)
                .arg(&prefix);
            let output = run_cancellable(&mut command, cancelled).map_err(|error| {
                if error == "index cancellation requested" { error }
                else { format!("无法启动 PDF 渲染器：{error}") }
            });
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    let _ = fs::remove_dir_all(&root);
                    return Err(error);
                }
            };
            if !output.status.success() {
                result.errors.push(format!("第 {page} 页渲染失败：{}", String::from_utf8_lossy(&output.stderr).trim()));
                continue;
            }
            let exact = root.join(format!("page-{page}.png"));
            if exact.is_file() { exact } else {
                fs::read_dir(&root).ok().and_then(|items| items.flatten().map(|entry| entry.path()).find(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))).unwrap_or(exact)
            }
        } else {
            let mut command = hidden_command(&renderer);
            command.args(["draw", "-F", "png", "-r", "180", "-o"])
                .arg(root.join(format!("page-{page}.png")))
                .arg(pdf_path)
                .arg(page.to_string());
            let output = run_cancellable(&mut command, cancelled).map_err(|error| {
                if error == "index cancellation requested" { error }
                else { format!("无法启动 MuPDF：{error}") }
            });
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    let _ = fs::remove_dir_all(&root);
                    return Err(error);
                }
            };
            if !output.status.success() {
                result.errors.push(format!("第 {page} 页渲染失败：{}", String::from_utf8_lossy(&output.stderr).trim()));
                continue;
            }
            root.join(format!("page-{page}.png"))
        };
        if !image_path.is_file() {
            result.errors.push(format!("第 {page} 页未生成预览图"));
            continue;
        }
        match tesseract_text(&exe, &image_path, cancelled) {
            Ok(text) => result.pages.push((page, text)),
            Err(error) if error == "index cancellation requested" => {
                let _ = fs::remove_dir_all(&root);
                return Err(error);
            }
            Err(error) => result.errors.push(format!("第 {page} 页 OCR 失败：{error}")),
        }
        let _ = fs::remove_file(image_path);
    }
    let _ = fs::remove_dir_all(&root);
    Ok(result)
}

fn normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn model_directory_ready(path: &Path) -> bool {
    path.is_dir()
        && path.join("config.json").is_file()
        && ["model.safetensors", "pytorch_model.bin", "model.safetensors.index.json", "pytorch_model.bin.index.json"]
            .iter()
            .any(|name| path.join(name).is_file())
}

fn likely_ocr_model_entry(path: &Path) -> bool {
    let file_hint = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let config = std::fs::read_to_string(path.join("config.json")).unwrap_or_default();
    let hint = format!("{}\n{}", file_hint, config).to_ascii_lowercase();
    ["teleocr", "trocr", "ocr", "text_recognition", "vision_encoder_decoder"]
        .iter()
        .any(|needle| hint.contains(needle))
}

fn detected_model_candidates(config: &crate::config::ModelConfig, roots: &[PathBuf]) -> Vec<String> {
    // Include the configured OCR directory's parent so a manually imported
    // model is discoverable even when it lives outside the default model store.
    let mut scan_roots = roots.to_vec();
    if let Some(model_dir) = clean_path(&config.teleocr_model_dir) {
        if let Some(parent) = model_dir.parent() {
            scan_roots.push(parent.to_path_buf());
        }
    }
    let mut candidates = crate::models::scan(config, &scan_roots)
        .into_iter()
        .filter(|entry| entry.format == "safetensors" && likely_ocr_model_entry(Path::new(&entry.model_dir)))
        .map(|entry| entry.model_dir)
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| path.to_ascii_lowercase());
    candidates.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    candidates
}

fn probe(python: &Path, code_dir: Option<&Path>) -> Result<ProbeResult, String> {
    let script = "import importlib, json, sys; p=sys.argv[1] if len(sys.argv)>1 else ''; sys.path.insert(0,p) if p else None; names=['torch','transformers','fitz','PIL','TeleOCR.vlm_utils.TeleOCR_model']; missing=[]; errors=[]\nfor n in names:\n try: importlib.import_module(n)\n except Exception as e: missing.append(n); errors.append(str(e))\nprint(json.dumps({'ok':not missing,'missing':missing,'detail':('Missing or broken Python modules: '+', '.join(missing)+' ('+'; '.join(errors)+')') if missing else 'Python, TeleOCR dependencies and PDF renderer are available'}))";
    let output = hidden_command(python)
        .args(["-c", &script])
        .arg(code_dir.map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
        .output()
        .map_err(|e| format!("无法启动 Python {}：{e}", python.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("Python 环境检查失败（退出码 {:?}）", output.status.code())
        } else {
            stderr
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("无法读取 Python 环境检查结果：{e}"))
}

fn probe_details(config: &crate::config::ModelConfig) -> (bool, bool, bool, String) {
    let python = python_path(&config.teleocr_python_path);
    let model = clean_path(&config.teleocr_model_dir);
    let code = clean_path(&config.teleocr_code_dir);
    let python_available = if python.components().count() > 1 || python.is_absolute() {
        python.is_file()
    } else {
        hidden_command(&python).arg("--version").output().is_ok()
    };
    let model_configured = model.as_ref().is_some_and(|p| model_directory_ready(p));
    // Tesseract is the default path when no TeleOCR model is selected. Avoid
    // starting Python or importing heavy OCR modules just to render that
    // status card; the optional runtime is probed only for a configured model.
    if !model_configured {
        return (
            python_available,
            false,
            false,
            "未选择 OCR 模型；将使用 Tesseract（如果已安装）".to_string(),
        );
    }
    if !python_available {
        return (false, false, model_configured, format!("未找到 Python 解释器：{}", python.display()));
    }
    if let Some(path) = code.as_ref() {
        if !path.is_dir() {
            return (true, false, model_configured, format!("TeleOCR 源码目录不存在：{}", path.display()));
        }
    }
    match probe(&python, code.as_deref()) {
        Ok(status) => (
            true,
            status.ok,
            model_configured,
            if model_configured {
                status.detail
            } else {
                format!("{}；尚未选择有效 OCR 模型目录", status.detail)
            },
        ),
        Err(error) => (true, false, model_configured, error),
    }
}

/// Returns per-page OCR text using one-based PDF page numbers.
pub fn extract_pdf_pages(
    model_dir: &str,
    pdf_path: &Path,
    pages_1_based: &[usize],
    python_executable: Option<&str>,
    teleocr_code_dir: Option<&str>,
) -> Result<OcrPdfResult, String> {
    extract_pdf_pages_with_cancel(
        model_dir,
        pdf_path,
        pages_1_based,
        python_executable,
        teleocr_code_dir,
        || false,
    )
}

pub fn extract_pdf_pages_with_cancel<F>(
    model_dir: &str,
    pdf_path: &Path,
    pages_1_based: &[usize],
    python_executable: Option<&str>,
    teleocr_code_dir: Option<&str>,
    cancelled: F,
) -> Result<OcrPdfResult, String>
where
    F: Fn() -> bool,
{
    if pages_1_based.is_empty() {
        return Ok(OcrPdfResult::default());
    }
    let model_dir = PathBuf::from(model_dir.trim());
    if !model_directory_ready(&model_dir) {
        return tesseract_pdf_pages(pdf_path, pages_1_based, &cancelled);
    }
    if !pdf_path.is_file() {
        return Err(format!("PDF 文件不存在：{}", pdf_path.display()));
    }
    let python = python_path(python_executable.unwrap_or_default());
    let code_dir = clean_path(teleocr_code_dir.unwrap_or_default()).unwrap_or_default();
    if !probe(&python, Some(&code_dir)).is_ok_and(|status| status.ok) {
        return tesseract_pdf_pages(pdf_path, pages_1_based, &cancelled);
    }

    let mut guard = worker_slot()
        .lock()
        .map_err(|_| "OCR worker 锁已损坏".to_string())?;
    ensure_worker(&mut guard, &python, &model_dir, &code_dir, &cancelled)?;
    let request = json!({ "pdf_path": pdf_path, "pages": pages_1_based });
    let response = {
        let worker = guard.as_mut().expect("worker initialized above");
        worker.send(&request)?;
        worker.receive(&cancelled)
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            *guard = None;
            return Err(error);
        }
    };
    if !response.ok {
        return Err(worker_error("TeleOCR 页面识别失败", &response));
    }
    Ok(OcrPdfResult {
        pages: response.pages.into_iter().map(|page| (page.page, page.text)).collect(),
        errors: response.errors,
    })
}

/// Extracts text from one image with the same lazily loaded TeleOCR worker.
pub fn extract_image_text(
    model_dir: &str,
    image_path: &Path,
    python_executable: Option<&str>,
    teleocr_code_dir: Option<&str>,
) -> Result<String, String> {
    extract_image_text_with_cancel(model_dir, image_path, python_executable, teleocr_code_dir, || false)
}

pub fn extract_image_text_with_cancel<F>(
    model_dir: &str,
    image_path: &Path,
    python_executable: Option<&str>,
    teleocr_code_dir: Option<&str>,
    cancelled: F,
) -> Result<String, String>
where
    F: Fn() -> bool,
{
    if !image_path.is_file() {
        return Err(format!("图片文件不存在：{}", image_path.display()));
    }
    let model_dir_path = PathBuf::from(model_dir.trim());
    if !model_directory_ready(&model_dir_path) {
        return tesseract_image(image_path, &cancelled);
    }
    let python = python_path(python_executable.unwrap_or_default());
    let code_dir = clean_path(teleocr_code_dir.unwrap_or_default()).unwrap_or_default();
    if !probe(&python, Some(&code_dir)).is_ok_and(|status| status.ok) {
        return tesseract_image(image_path, &cancelled);
    }
    let mut guard = worker_slot()
        .lock()
        .map_err(|_| "OCR worker 锁已损坏".to_string())?;
    ensure_worker(&mut guard, &python, &model_dir_path, &code_dir, &cancelled)?;
    let request = json!({ "op": "image", "image_path": image_path });
    let response = {
        let worker = guard.as_mut().expect("worker initialized above");
        worker.send(&request)?;
        worker.receive(&cancelled)
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            *guard = None;
            return Err(error);
        }
    };
    if !response.ok {
        return Err(worker_error("TeleOCR 图片识别失败", &response));
    }
    if !response.errors.is_empty() {
        return Err(response.errors.join("; "));
    }
    Ok(response.text)
}

pub fn is_configured(model_dir: &str, python_executable: Option<&str>, teleocr_code_dir: Option<&str>) -> bool {
    let model_ok = clean_path(model_dir).is_some_and(|p| model_directory_ready(&p));
    let python = python_path(python_executable.unwrap_or_default());
    let code = clean_path(teleocr_code_dir.unwrap_or_default());
    model_ok && probe(&python, code.as_deref()).is_ok_and(|status| status.ok)
}

/// Terminates the optional long-lived Python worker on application shutdown.
pub fn shutdown() {
    if let Some(slot) = WORKER.get() {
        if let Ok(mut worker) = slot.lock() {
            *worker = None;
        }
    }
}

#[tauri::command]
pub fn teleocr_status(state: State<'_, std::sync::Arc<crate::state::AppState>>) -> OcrStatus {
    let config = state.cfg.read().model.clone();
    let cache_key = format!(
        "{}\n{}\n{}",
        config.teleocr_model_dir, config.teleocr_python_path, config.teleocr_code_dir
    );
    if let Some(cached) = OCR_STATUS_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|item| (item.key.clone(), item.created, item.value.clone())))
    {
        if cached.0 == cache_key && cached.1.elapsed() < OCR_STATUS_CACHE_TTL {
            return cached.2;
        }
    }
    let (python_available, teleocr_available, model_configured, detail) = probe_details(&config);
    let (tesseract_available, pdf_renderer_available, tesseract_detail) = tesseract_status();
    let model_root = crate::models::model_dir(&state.app_dir, &state.data_dir);
    let model_candidates = detected_model_candidates(&config, std::slice::from_ref(&model_root));
    let mut missing_dependencies = Vec::new();
    if model_configured && !teleocr_available {
        if !python_available {
            missing_dependencies.push("Python sidecar".to_string());
        }
        let lower = detail.to_ascii_lowercase();
        for (needle, label) in [
            ("torch", "PyTorch"),
            ("transformers", "Transformers"),
            ("fitz", "PyMuPDF"),
            ("pil", "Pillow"),
            ("teleocr", "TeleOCR"),
        ] {
            if lower.contains(needle) && !missing_dependencies.iter().any(|item| item == label) {
                missing_dependencies.push(label.to_string());
            }
        }
        if missing_dependencies.is_empty() {
            missing_dependencies.push("TeleOCR 运行依赖".to_string());
        }
    }
    let detail = if model_configured || tesseract_available {
        detail
    } else {
        format!("{}；{}", detail, tesseract_detail)
    };
    let status = OcrStatus {
        python_available,
        teleocr_available,
        model_configured,
        tesseract_available,
        pdf_renderer_available,
        model_candidates,
        missing_dependencies,
        python_path: config.teleocr_python_path,
        code_dir: config.teleocr_code_dir,
        model_dir: config.teleocr_model_dir,
        detail,
    };
    if let Ok(mut guard) = OCR_STATUS_CACHE.get_or_init(|| Mutex::new(None)).lock() {
        *guard = Some(OcrStatusCache { key: cache_key, created: Instant::now(), value: status.clone() });
    }
    status
}
