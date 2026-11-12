use crate::archive::{collect_volumes, detect_kind, ArchiveKind};
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Where to find the external `7zz` and `unrar` binaries.
///
/// Resolution order:
/// 1. `UNPACK_TOOLS_DIR` environment variable (when `tools_dir` is `None`).
/// 2. The caller-provided `tools_dir`.
/// 3. Next to the current executable (matches Tauri sidecars and a CLI
///    distributed together with its tools).
/// 4. `PATH`.
pub struct ToolLocator {
    pub tools_dir: Option<PathBuf>,
}

impl ToolLocator {
    pub fn new(tools_dir: Option<PathBuf>) -> Self {
        Self { tools_dir }
    }

    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        if let Some(dir) = &self.tools_dir {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }

        if let Ok(dir) = std::env::var("UNPACK_TOOLS_DIR") {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }

        which(name).map(PathBuf::from).with_context(|| {
            format!("找不到 {name}。请将 7zz 和 unrar 放到程序同目录，或加入 PATH。")
        })
    }
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub archive: PathBuf,
    pub out_dir: PathBuf,
    pub tools_dir: Option<PathBuf>,
    pub delete_after: bool,
    /// When set to `true` while a tool is running, the process is killed and
    /// extraction stops with an "已取消解压" error.
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    SevenZip,
    Unrar,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractEvent {
    pub percent: f32,
    pub bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<f64>,
    pub eta_secs: Option<u64>,
    pub file: Option<String>,
    pub message: Option<String>,
}

pub fn extract(options: ExtractOptions, mut on_event: impl FnMut(ExtractEvent)) -> Result<()> {
    let kind = detect_kind(&options.archive)
        .ok_or_else(|| anyhow!("无法识别的压缩包格式：{}", options.archive.display()))?;

    let volumes = collect_volumes(&options.archive);
    std::fs::create_dir_all(&options.out_dir)
        .with_context(|| format!("无法创建输出目录：{}", options.out_dir.display()))?;

    let locator = ToolLocator::new(options.tools_dir.clone());
    let (total_bytes, file_sizes) = archive_info(&locator, kind, &options.archive);

    if let Some(total) = total_bytes {
        if let Some(available) = available_space(&options.out_dir) {
            if available < total {
                return Err(anyhow!(
                    "目标磁盘剩余空间不足：需要约 {}，可用 {}",
                    human_bytes(total),
                    human_bytes(available)
                ));
            }
        }
    }

    let candidates = candidate_tools(kind, &locator, &options.archive)?;

    let mut last_error: Option<anyhow::Error> = None;
    for (tool, tool_kind) in candidates {
        match try_extract(
            &options,
            &tool,
            tool_kind,
            total_bytes,
            &file_sizes,
            &mut on_event,
        ) {
            Ok(()) => {
                if options.delete_after {
                    delete_volumes(&volumes, &mut on_event);
                }
                on_event(ExtractEvent {
                    percent: 100.0,
                    bytes: total_bytes,
                    total_bytes,
                    speed_bps: None,
                    eta_secs: Some(0),
                    file: None,
                    message: Some(if options.delete_after {
                        "解压完成，原压缩包已删除".to_string()
                    } else {
                        "解压完成".to_string()
                    }),
                });
                return Ok(());
            }
            Err(err) => {
                let retryable = tool_kind == ToolKind::SevenZip
                    && kind == ArchiveKind::Rar
                    && is_unsupported_error(&err);
                if !retryable {
                    return Err(err);
                }
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("没有可用的解压程序（7zz / unrar）")))
}

fn candidate_tools(
    kind: ArchiveKind,
    locator: &ToolLocator,
    archive: &Path,
) -> Result<Vec<(PathBuf, ToolKind)>> {
    match kind {
        ArchiveKind::SevenZip | ArchiveKind::Zip => {
            Ok(vec![(locator.resolve("7zz")?, ToolKind::SevenZip)])
        }
        ArchiveKind::Rar => {
            let mut candidates = Vec::new();
            if let Ok(seven) = locator.resolve("7zz") {
                if seven_can_list(&seven, archive) {
                    candidates.push((seven, ToolKind::SevenZip));
                }
            }
            if let Ok(unrar) = locator.resolve("unrar") {
                candidates.push((unrar, ToolKind::Unrar));
            }
            if candidates.is_empty() {
                return Err(anyhow!("找不到可用的解压程序（7zz / unrar）"));
            }
            Ok(candidates)
        }
    }
}

fn is_unsupported_error(err: &anyhow::Error) -> bool {
    format!("{err:#}").to_lowercase().contains("unsupported")
}

fn archive_info(
    locator: &ToolLocator,
    kind: ArchiveKind,
    archive: &Path,
) -> (Option<u64>, HashMap<String, u64>) {
    if let Ok(seven) = locator.resolve("7zz") {
        if let Some(files) = collect_file_sizes(&seven, archive) {
            let total: u64 = files.values().copied().sum();
            if total > 0 {
                return (Some(total), files);
            }
        }
    }

    if kind == ArchiveKind::Rar {
        if let Ok(unrar) = locator.resolve("unrar") {
            if let Ok(total) = detect_total_size(&unrar, ToolKind::Unrar, archive) {
                return (Some(total), HashMap::new());
            }
        }
    }

    (None, HashMap::new())
}

fn collect_file_sizes(seven: &Path, archive: &Path) -> Option<HashMap<String, u64>> {
    let output = Command::new(seven)
        .arg("l")
        .arg("-slt")
        .arg(archive)
        .arg("-y")
        .arg("-bso1")
        .arg("-bse1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let mut files = HashMap::new();
    let mut current_path: Option<String> = None;
    let mut in_file_list = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "----------" {
            in_file_list = true;
            continue;
        }
        if !in_file_list {
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("Path = ") {
            current_path = Some(path.to_string());
        } else if let Some(size) = trimmed.strip_prefix("Size = ") {
            if let Some(path) = current_path.take() {
                if let Ok(size) = size.trim().parse::<u64>() {
                    files.insert(path, size);
                }
            }
        }
    }

    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

fn try_extract(
    options: &ExtractOptions,
    tool: &Path,
    tool_kind: ToolKind,
    total_bytes: Option<u64>,
    file_sizes: &HashMap<String, u64>,
    on_event: &mut impl FnMut(ExtractEvent),
) -> Result<()> {
    on_event(ExtractEvent {
        percent: 0.0,
        bytes: Some(0),
        total_bytes,
        speed_bps: None,
        eta_secs: None,
        file: None,
        message: Some(format!("正在解压 {} ...", options.archive.display())),
    });

    let mut cmd = Command::new(tool);
    match tool_kind {
        ToolKind::SevenZip => {
            cmd.arg("x")
                .arg(&options.archive)
                .arg(format!("-o{}", options.out_dir.display()))
                .arg("-y")
                .arg("-bsp1")
                .arg("-bso1")
                .arg("-bse1");
        }
        ToolKind::Unrar => {
            cmd.arg("x")
                .arg("-o+")
                .arg("-y")
                .arg(&options.archive)
                .arg(&options.out_dir);
        }
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("无法启动解压程序（{}）", tool.display()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法读取解压程序输出"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("无法读取解压程序错误输出"))?;

    let child_slot = Arc::new(Mutex::new(Some(child)));
    let watcher = options.cancel.as_ref().map(|cancel| {
        let cancel = Arc::clone(cancel);
        let slot = Arc::clone(&child_slot);
        std::thread::spawn(move || loop {
            if cancel.load(Ordering::SeqCst) {
                let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(child) = guard.as_mut() {
                    let _ = child.kill();
                }
                break;
            }
            if slot.lock().map_or(true, |guard| guard.is_none()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        })
    });

    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_thread = {
        let stderr_buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut buf) = stderr_buf.lock() {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
        })
    };

    let stdout_tail = read_progress(
        stdout,
        total_bytes,
        tool_kind,
        file_sizes,
        &options.out_dir,
        &mut *on_event,
    );
    let status = {
        let mut guard = child_slot.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .take()
            .ok_or_else(|| anyhow!("解压进程已退出"))?
            .wait()
            .context("等待解压进程结束时出错")?
    };
    stderr_thread.join().ok();
    if let Some(watcher) = watcher {
        watcher.join().ok();
    }

    if options
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.load(Ordering::SeqCst))
    {
        return Err(anyhow!("已取消解压"));
    }

    if !status.success() {
        let stderr_tail = {
            let buf = stderr_buf.lock().unwrap_or_else(|e| e.into_inner());
            tail_text(&buf, 2000)
        };
        let tail = if stderr_tail.trim().is_empty() {
            tail_text(&stdout_tail, 2000)
        } else {
            stderr_tail
        };
        return Err(anyhow!(
            "解压失败（{}）：{}",
            options.archive.display(),
            tail
        ));
    }

    Ok(())
}

fn delete_volumes(volumes: &[PathBuf], on_event: &mut impl FnMut(ExtractEvent)) {
    for volume in volumes {
        if let Err(err) = std::fs::remove_file(volume) {
            if err.kind() != std::io::ErrorKind::NotFound {
                on_event(ExtractEvent {
                    percent: 100.0,
                    bytes: None,
                    total_bytes: None,
                    speed_bps: None,
                    eta_secs: Some(0),
                    file: None,
                    message: Some(format!("删除原文件失败：{err}")),
                });
            }
        }
    }
}

fn read_progress(
    stdout: impl Read + Send + 'static,
    total_bytes: Option<u64>,
    tool_kind: ToolKind,
    file_sizes: &HashMap<String, u64>,
    out_dir: &Path,
    on_event: &mut impl FnMut(ExtractEvent),
) -> Vec<u8> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut raw: Vec<u8> = Vec::with_capacity(32 * 1024);
    let mut tracker = UnrarTracker::new(file_sizes, out_dir);

    let start = Instant::now();
    let mut last_sample = start;
    let mut last_emit = start;
    let mut last_emitted_percent = -1.0f32;
    let mut last_emitted_bytes = 0u64;
    let mut last_bytes = 0u64;
    let mut smooth_speed = 0.0f64;

    loop {
        let now = Instant::now();
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                raw.extend_from_slice(&chunk);
                if raw.len() > 32 * 1024 {
                    let overflow = raw.len() - 32 * 1024;
                    raw.drain(..overflow);
                }
                if tool_kind == ToolKind::Unrar {
                    tracker.feed(&chunk);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let (bytes, hint_percent) = match tool_kind {
            ToolKind::SevenZip => {
                let percent = latest_percent(&raw).unwrap_or(0.0).clamp(0.0, 100.0);
                let bytes = total_bytes.map(|total| (total as f64 * percent as f64 / 100.0) as u64);
                (bytes, percent)
            }
            ToolKind::Unrar => (Some(tracker.bytes()), tracker.percent()),
        };

        let bytes = bytes.map(|value| {
            let cap = total_bytes.unwrap_or(u64::MAX);
            value.min(cap).max(last_bytes)
        });
        let percent = match (total_bytes, bytes) {
            (Some(total), Some(bytes)) if total > 0 => {
                (bytes as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
            }
            _ => hint_percent.clamp(0.0, 100.0),
        };

        let elapsed = now.duration_since(last_sample);
        if let Some(bytes) = bytes {
            if elapsed >= Duration::from_millis(200) && bytes > last_bytes {
                let inst = (bytes - last_bytes) as f64 / elapsed.as_secs_f64();
                smooth_speed = if smooth_speed == 0.0 {
                    inst
                } else {
                    smooth_speed * 0.6 + inst * 0.4
                };
                last_bytes = bytes;
                last_sample = now;
            }
        }

        let speed = (smooth_speed > 0.0).then_some(smooth_speed);
        let eta = match (total_bytes, bytes, speed) {
            (Some(total), Some(bytes), Some(speed)) => {
                Some(((total.saturating_sub(bytes)) as f64 / speed) as u64)
            }
            _ => None,
        };

        let should_emit = percent >= 100.0
            || (bytes.unwrap_or(0) > last_emitted_bytes
                && now.duration_since(last_emit) >= Duration::from_millis(250))
            || (percent - last_emitted_percent) >= 0.1;
        if should_emit {
            last_emitted_percent = percent;
            last_emitted_bytes = bytes.unwrap_or(last_emitted_bytes);
            last_emit = now;
            on_event(ExtractEvent {
                percent,
                bytes,
                total_bytes,
                speed_bps: speed,
                eta_secs: eta,
                file: None,
                message: None,
            });
        }
    }

    raw
}

#[derive(Default)]
struct SizeIndex {
    by_path: HashMap<String, u64>,
    by_name: HashMap<String, Vec<(String, u64)>>,
}

impl SizeIndex {
    fn new(files: &HashMap<String, u64>) -> Self {
        let mut index = Self::default();
        for (path, size) in files {
            let key = normalize_path(path);
            index.by_path.insert(key.clone(), *size);
            let name = key.rsplit('/').next().unwrap_or(&key).to_string();
            index.by_name.entry(name).or_default().push((key, *size));
        }
        index
    }

    fn lookup(&self, output_path: &str, out_dir: &Path) -> Option<u64> {
        let path = normalize_path(output_path);
        let out_dir = normalize_path(&out_dir.to_string_lossy());
        let relative = path
            .strip_prefix(&out_dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(&path);

        if let Some(size) = self.by_path.get(relative) {
            return Some(*size);
        }
        if let Some(size) = self.by_path.get(&path) {
            return Some(*size);
        }

        let name = path.rsplit('/').next()?;
        let candidates = self.by_name.get(name)?;
        if candidates.len() == 1 {
            return Some(candidates[0].1);
        }
        candidates
            .iter()
            .find(|(key, _)| path == *key || path.ends_with(&format!("/{key}")))
            .map(|(_, size)| *size)
    }
}

struct CurrentFile {
    key: String,
    output: PathBuf,
    planned_size: Option<u64>,
    percent_hint: f32,
}

struct UnrarTracker {
    line: Vec<u8>,
    current: Option<CurrentFile>,
    completed: HashSet<String>,
    completed_bytes: u64,
    sizes: SizeIndex,
    out_dir: PathBuf,
}

impl UnrarTracker {
    fn new(file_sizes: &HashMap<String, u64>, out_dir: &Path) -> Self {
        Self {
            line: Vec::with_capacity(512),
            current: None,
            completed: HashSet::new(),
            completed_bytes: 0,
            sizes: SizeIndex::new(file_sizes),
            out_dir: out_dir.to_path_buf(),
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            match byte {
                b'\n' => {
                    self.refresh();
                    if self.line_is_complete() {
                        self.finish_current();
                    }
                    self.line.clear();
                }
                b'\r' => {
                    self.refresh();
                    self.line.clear();
                }
                0x08 => {
                    self.line.pop();
                }
                _ => {
                    if self.line.len() < 64 * 1024 {
                        self.line.push(byte);
                    }
                }
            }
        }
        self.refresh();
    }

    fn refresh(&mut self) {
        let text = String::from_utf8_lossy(&self.line);
        let Some((name, percent)) = parse_unrar_extracting(&text) else {
            return;
        };

        if self
            .current
            .as_ref()
            .is_none_or(|current| current.key != name)
        {
            self.finish_current();
            let planned_size = self.sizes.lookup(&name, &self.out_dir);
            let output = if Path::new(&name).is_absolute() {
                PathBuf::from(&name)
            } else {
                self.out_dir.join(&name)
            };
            self.current = Some(CurrentFile {
                key: name.clone(),
                output,
                planned_size,
                percent_hint: percent.unwrap_or(0.0),
            });
        } else if let Some(percent) = percent {
            if let Some(current) = self.current.as_mut() {
                current.percent_hint = current.percent_hint.max(percent);
            }
        }
    }

    fn line_is_complete(&self) -> bool {
        if self.current.is_none() {
            return false;
        }
        let text = String::from_utf8_lossy(&self.line);
        let text = text.trim_end();
        text.starts_with("Extracting") && (text.ends_with("OK") || self.current_percent() >= 99.5)
    }

    fn finish_current(&mut self) {
        let Some(current) = self.current.take() else {
            return;
        };
        if !self.completed.insert(current.key) {
            return;
        }
        let written = file_len(&current.output);
        self.completed_bytes = self
            .completed_bytes
            .saturating_add(current.planned_size.unwrap_or(written));
    }

    fn current_written(&self) -> u64 {
        let Some(current) = self.current.as_ref() else {
            return 0;
        };
        let cap = current.planned_size.unwrap_or(u64::MAX);
        let written = file_len(&current.output).min(cap);
        if written > 0 {
            return written;
        }
        current
            .planned_size
            .map(|size| (size as f64 * current.percent_hint as f64 / 100.0) as u64)
            .unwrap_or(0)
    }

    fn bytes(&self) -> u64 {
        self.completed_bytes.saturating_add(self.current_written())
    }

    fn current_percent(&self) -> f32 {
        let Some(current) = self.current.as_ref() else {
            return 0.0;
        };
        match current.planned_size {
            Some(size) if size > 0 => {
                (self.current_written() as f64 / size as f64 * 100.0).clamp(0.0, 100.0) as f32
            }
            _ => current.percent_hint,
        }
    }

    fn percent(&self) -> f32 {
        self.current_percent()
    }
}

fn parse_unrar_extracting(text: &str) -> Option<(String, Option<f32>)> {
    let rest = text.trim_start().strip_prefix("Extracting")?.trim_start();
    let rest = rest.trim_end();
    if rest.is_empty() {
        return None;
    }

    if let Some((percent, start)) = percent_at_end(rest) {
        let name = rest[..start].trim_end();
        if name.is_empty() {
            return None;
        }
        return Some((name.to_string(), Some(percent)));
    }

    let name = rest.strip_suffix("OK").unwrap_or(rest).trim_end();
    if name.is_empty() {
        None
    } else {
        Some((name.to_string(), None))
    }
}

fn percent_at_end(text: &str) -> Option<(f32, usize)> {
    let percent_idx = text.rfind('%')?;
    let before = &text[..percent_idx];
    let bytes = before.as_bytes();
    let mut start = bytes.len();
    while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.') {
        start -= 1;
    }
    if start == bytes.len() {
        return None;
    }
    let value = before[start..].parse::<f32>().ok()?;
    if !(0.0..=100.0).contains(&value) {
        return None;
    }
    Some((value, start))
}

fn normalize_path(raw: &str) -> String {
    let mut path = raw.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    if path.len() > 1 {
        path = path.trim_end_matches('/').to_string();
    }
    path
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn latest_percent(window: &[u8]) -> Option<f32> {
    let text = String::from_utf8_lossy(window);
    let mut best: Option<f32> = None;
    for idx in text.match_indices('%').map(|(i, _)| i) {
        let before = &text[..idx];
        let mut j = before.len();
        while j > 0 && before.as_bytes()[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < before.len() {
            if let Ok(num) = before[j..].parse::<f32>() {
                best = Some(num);
            }
        }
    }
    best
}

fn seven_can_list(seven: &Path, archive: &Path) -> bool {
    Command::new(seven)
        .arg("l")
        .arg(archive)
        .arg("-y")
        .arg("-bso1")
        .arg("-bse1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn detect_total_size(tool: &Path, tool_kind: ToolKind, archive: &Path) -> Result<u64> {
    let mut cmd = Command::new(tool);
    match tool_kind {
        ToolKind::SevenZip => {
            cmd.arg("l")
                .arg("-slt")
                .arg(archive)
                .arg("-y")
                .arg("-bso1")
                .arg("-bse1");
        }
        ToolKind::Unrar => {
            cmd.arg("lt").arg(archive).arg("-y");
        }
    }

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("读取压缩包信息失败")?;
    if !output.status.success() {
        return Err(anyhow!("读取压缩包信息失败"));
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut total = 0u64;
    match tool_kind {
        ToolKind::SevenZip => {
            // In `7zz l -slt`, the file list starts after a line of ten
            // dashes. Only `Size =` lines after that belong to real files;
            // the earlier block describes the outer/inner archive itself.
            let mut in_file_list = false;
            for line in text.lines() {
                if line.trim() == "----------" {
                    in_file_list = true;
                    continue;
                }
                if in_file_list {
                    if let Some(rest) = line.trim().strip_prefix("Size = ") {
                        if let Ok(n) = rest.trim().parse::<u64>() {
                            total += n;
                        }
                    }
                }
            }
        }
        ToolKind::Unrar => {
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("Size:") {
                    if let Ok(n) = rest.trim().parse::<u64>() {
                        total += n;
                    }
                }
            }
        }
    }

    if total == 0 {
        Err(anyhow!("未能从压缩包信息解析总大小"))
    } else {
        Ok(total)
    }
}

fn tail_text(buf: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(buf);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.into_owned()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(unix)]
fn available_space(dir: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    let block_size = stat.f_frsize as u64;
    let available_blocks = stat.f_bavail as u64;
    Some(available_blocks.saturating_mul(block_size))
}

#[cfg(not(unix))]
fn available_space(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_tool(dir: &Path, name: &str, exit_code: &str) {
        let script = format!("#!/bin/sh\nexit {exit_code}\n");
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }

    fn write_script(dir: &Path, name: &str, body: &str) {
        let script = format!("#!/bin/sh\n{body}\n");
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }

    #[test]
    fn rar_uses_7zz_when_supported() {
        let dir = std::env::temp_dir().join(format!("unpack-engine-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_tool(&dir, "7zz", "0");
        write_tool(&dir, "unrar", "1");
        let archive = dir.join("movie.part01.rar");
        fs::write(&archive, b"x").unwrap();

        let locator = ToolLocator::new(Some(dir.clone()));
        let candidates = candidate_tools(ArchiveKind::Rar, &locator, &archive).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].1, ToolKind::SevenZip);
        assert!(candidates[0].0.ends_with("7zz"));
        assert_eq!(candidates[1].1, ToolKind::Unrar);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rar_falls_back_to_unrar_for_new_format() {
        let dir = std::env::temp_dir().join(format!("unpack-engine-unrar-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_tool(&dir, "7zz", "1");
        write_tool(&dir, "unrar", "1");
        let archive = dir.join("movie.part01.rar");
        fs::write(&archive, b"x").unwrap();

        let locator = ToolLocator::new(Some(dir.clone()));
        let candidates = candidate_tools(ArchiveKind::Rar, &locator, &archive).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, ToolKind::Unrar);
        assert!(candidates[0].0.ends_with("unrar"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rar_unsupported_method_retries_with_unrar() {
        let dir = std::env::temp_dir().join(format!("unpack-retry-{}", std::process::id()));
        let tools = dir.join("tools");
        let out = dir.join("out");
        fs::create_dir_all(&tools).unwrap();

        write_script(
            &tools,
            "7zz",
            r#"
if [ "$1" = "l" ]; then
  exit 0
fi
echo "ERROR: Unsupported Method : bad.prx" >&2
exit 2
"#,
        );
        write_script(
            &tools,
            "unrar",
            r#"
if [ "$1" = "lt" ]; then
  echo "Size: 123"
  exit 0
fi
touch "$5/ran"
exit 0
"#,
        );

        let archive = dir.join("movie.part01.rar");
        fs::write(&archive, b"x").unwrap();

        let options = ExtractOptions {
            archive,
            out_dir: out.clone(),
            tools_dir: Some(tools),
            delete_after: false,
            cancel: None,
        };
        let mut events = Vec::new();
        extract(options, |event| events.push(event)).unwrap();

        assert!(out.join("ran").is_file());
        assert!(events
            .iter()
            .filter(|e| e.percent >= 100.0)
            .any(|e| e.message.as_deref() == Some("解压完成")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive_info_keeps_listing_when_7zz_reports_warnings() {
        let dir = std::env::temp_dir().join(format!("unpack-list-{}", std::process::id()));
        let tools = dir.join("tools");
        fs::create_dir_all(&tools).unwrap();
        write_script(
            &tools,
            "7zz",
            r#"
if [ "$1" = "l" ]; then
  cat <<'EOF'
----------
Path = PPSA/a.bin
Size = 100
Path = PPSA/b.bin
Size = 200
EOF
  exit 2
fi
exit 1
"#,
        );

        let archive = dir.join("movie.part01.rar");
        fs::write(&archive, b"x").unwrap();
        let locator = ToolLocator::new(Some(tools));
        let (total, files) = archive_info(&locator, ArchiveKind::Rar, &archive);

        assert_eq!(total, Some(300));
        assert_eq!(files.len(), 2);
        assert_eq!(files.get("PPSA/b.bin"), Some(&200));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sevenzip_progress_uses_archive_total() {
        let mut events = Vec::new();
        read_progress(
            std::io::Cursor::new(b"  0%\x08\x08\x08   50%\x08\x08\x08  100%".to_vec()),
            Some(1_000),
            ToolKind::SevenZip,
            &HashMap::new(),
            Path::new("/tmp"),
            &mut |event| events.push(event),
        );

        let last = events.last().expect("progress event");
        assert_eq!(last.percent, 100.0);
        assert_eq!(last.bytes, Some(1_000));
        assert_eq!(last.total_bytes, Some(1_000));
    }

    #[test]
    fn unrar_progress_uses_output_bytes_and_planned_sizes() {
        let dir = std::env::temp_dir().join(format!("unpack-tracker-{}", std::process::id()));
        let out = dir.join("out");
        let files_dir = out.join("PPSA03016-app");
        fs::create_dir_all(&files_dir).unwrap();
        fs::write(files_dir.join("a.bin"), vec![0u8; 1_000]).unwrap();
        fs::write(files_dir.join("b.bin"), vec![0u8; 300]).unwrap();

        let mut sizes = HashMap::new();
        sizes.insert("PPSA03016-app/a.bin".to_string(), 1_000);
        sizes.insert("PPSA03016-app/b.bin".to_string(), 2_000);
        let mut tracker = UnrarTracker::new(&sizes, &out);

        let first = format!(
            "Extracting  {}/PPSA03016-app/a.bin                    \x08\x08\x08\x08  0%\x08\x08\x08\x08\x08  OK \n",
            out.display()
        );
        tracker.feed(first.as_bytes());
        assert_eq!(tracker.completed_bytes, 1_000);
        assert!(tracker.current.is_none());

        let second = format!(
            "Extracting  {}/PPSA03016-app/b.bin                    \x08\x08\x08\x08  0%",
            out.display()
        );
        tracker.feed(second.as_bytes());
        assert_eq!(tracker.bytes(), 1_300);
        assert_eq!(tracker.current.as_ref().unwrap().planned_size, Some(2_000));

        let _ = fs::remove_dir_all(&dir);
    }
}
