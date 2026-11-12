use serde::Serialize;
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use unpack_core::{detect_kind, extract, ArchiveKind, ExtractEvent, ExtractOptions, ToolLocator};

const PROGRESS_EVENT: &str = "unpack://progress";
const VERIFY_OUTPUT: &str = "verify://output";
const VERIFY_DONE: &str = "verify://done";
const VERIFY_BEGIN: &str = "verify://begin";

struct AppState {
    extracting: AtomicBool,
    cancel: Arc<AtomicBool>,
    verify_archive: Mutex<Option<String>>,
    verifying: AtomicBool,
    verify_cancel: Arc<AtomicBool>,
}

#[derive(Clone, Serialize)]
struct ProgressPayload {
    percent: f32,
    bytes: Option<u64>,
    total_bytes: Option<u64>,
    speed_bps: Option<f64>,
    eta_secs: Option<u64>,
    message: Option<String>,
    done: bool,
    error: Option<String>,
}

impl ProgressPayload {
    fn from_event(event: ExtractEvent) -> Self {
        Self {
            percent: event.percent,
            bytes: event.bytes,
            total_bytes: event.total_bytes,
            speed_bps: event.speed_bps,
            eta_secs: event.eta_secs,
            message: event.message,
            done: false,
            error: None,
        }
    }

    fn done(last: Option<ExtractEvent>) -> Self {
        let bytes = last.as_ref().and_then(|event| event.bytes);
        let total_bytes = last.as_ref().and_then(|event| event.total_bytes);
        Self {
            percent: 100.0,
            bytes,
            total_bytes,
            speed_bps: None,
            eta_secs: Some(0),
            message: Some("解压完成".to_string()),
            done: true,
            error: None,
        }
    }

    fn error(message: String) -> Self {
        Self {
            percent: 0.0,
            bytes: None,
            total_bytes: None,
            speed_bps: None,
            eta_secs: None,
            message: None,
            done: true,
            error: Some(message),
        }
    }
}

#[tauri::command]
async fn pick_archive(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .add_filter("压缩包", &["7z", "zip", "rar", "001", "z01"])
        .pick_file(move |file| {
            let _ = tx.send(file);
        });

    tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .ok()
        .flatten()
        .and_then(|file| file.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder);
    });

    tauri::async_runtime::spawn_blocking(move || rx.recv().ok().flatten())
        .await
        .ok()
        .flatten()
        .and_then(|folder| folder.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn start_extract(
    app: tauri::AppHandle,
    archive: String,
    out_dir: String,
    delete_after: bool,
) -> Result<(), String> {
    let archive_path = PathBuf::from(&archive);
    let out_path = PathBuf::from(&out_dir);
    if !archive_path.is_file() {
        return Err("压缩包文件不存在".to_string());
    }

    let state = app.state::<AppState>();
    if state.extracting.swap(true, Ordering::SeqCst) {
        return Err("已有解压任务正在进行".to_string());
    }
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.cancel);

    let handle = app.clone();
    std::thread::spawn(move || {
        let options = ExtractOptions {
            archive: archive_path,
            out_dir: out_path,
            tools_dir: None,
            delete_after,
            cancel: Some(Arc::clone(&cancel)),
        };

        let mut last_event = None;
        let result = extract(options, |event| {
            last_event = Some(event.clone());
            let _ = handle.emit(PROGRESS_EVENT, ProgressPayload::from_event(event));
        });

        handle
            .state::<AppState>()
            .extracting
            .store(false, Ordering::SeqCst);

        if cancel.load(Ordering::SeqCst) {
            if let Some(window) = handle.get_webview_window("main") {
                let _ = window.destroy();
            }
            return;
        }

        let payload = match result {
            Ok(()) => ProgressPayload::done(last_event),
            Err(err) => ProgressPayload::error(format!("{err:#}")),
        };
        let _ = handle.emit(PROGRESS_EVENT, payload);
    });

    Ok(())
}

#[derive(Clone, Serialize)]
struct VerifyLine {
    stream: &'static str,
    text: String,
}

#[derive(Clone, Serialize)]
struct VerifyDone {
    ok: bool,
    code: Option<i32>,
    error: Option<String>,
}

#[tauri::command]
fn open_verify_window(app: tauri::AppHandle, archive: String) -> Result<(), String> {
    let path = PathBuf::from(&archive);
    if !path.is_file() {
        return Err("压缩包文件不存在".to_string());
    }
    if detect_kind(&path).is_none() {
        return Err("无法识别的压缩包格式".to_string());
    }

    let state = app.state::<AppState>();
    *state
        .verify_archive
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(archive.clone());

    if state.verifying.load(Ordering::SeqCst) {
        if let Some(window) = app.get_webview_window("verify") {
            let _ = window.set_focus();
            return Ok(());
        }
        return Err("上一次校验仍在结束中，请稍后再试".to_string());
    }

    if let Some(window) = app.get_webview_window("verify") {
        let _ = window.set_focus();
        window
            .emit(VERIFY_BEGIN, archive)
            .map_err(|err| err.to_string())?;
        return Ok(());
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    WebviewWindowBuilder::new(&app, "verify", WebviewUrl::App("verify.html".into()))
        .title(format!("校验 - {name}"))
        .inner_size(760.0, 580.0)
        .min_inner_size(520.0, 380.0)
        .build()
        .map(|_| ())
        .map_err(|err| format!("无法打开校验窗口：{err}"))
}

#[tauri::command]
fn verify_start(app: tauri::AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let archive = state
        .verify_archive
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
        .ok_or_else(|| "没有待校验的压缩包".to_string())?;

    if state.verifying.swap(true, Ordering::SeqCst) {
        return Err("校验正在进行中".to_string());
    }
    state.verify_cancel.store(false, Ordering::SeqCst);
    let cancel = Arc::clone(&state.verify_cancel);

    let handle = app.clone();
    let archive_for_thread = archive.clone();
    std::thread::spawn(move || {
        let result = run_verify(&handle, &archive_for_thread, Arc::clone(&cancel));
        handle
            .state::<AppState>()
            .verifying
            .store(false, Ordering::SeqCst);

        let payload = match result {
            Ok(code) => VerifyDone {
                ok: code == 0,
                code: Some(code),
                error: None,
            },
            Err(err) => VerifyDone {
                ok: false,
                code: None,
                error: Some(err),
            },
        };
        let _ = handle.emit_to("verify", VERIFY_DONE, payload);
    });

    Ok(archive)
}

fn run_verify(
    app: &tauri::AppHandle,
    archive: &str,
    cancel: Arc<AtomicBool>,
) -> Result<i32, String> {
    let path = PathBuf::from(archive);
    let kind = detect_kind(&path).ok_or_else(|| "无法识别的压缩包格式".to_string())?;
    let tool_name = match kind {
        ArchiveKind::Rar => "unrar",
        ArchiveKind::SevenZip | ArchiveKind::Zip => "7zz",
    };
    let tool = ToolLocator::new(None)
        .resolve(tool_name)
        .map_err(|err| format!("{err:#}"))?;

    let mut cmd = std::process::Command::new(&tool);
    cmd.arg("t").arg("-y");
    if kind != ArchiveKind::Rar {
        cmd.arg("-bsp1").arg("-bb1");
    }
    cmd.arg(&path);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let _ = app.emit_to(
        "verify",
        VERIFY_OUTPUT,
        VerifyLine {
            stream: "meta",
            text: format!("$ {} t {}", tool.display(), archive),
        },
    );

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("无法启动校验程序：{err}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取校验输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取校验错误输出".to_string())?;

    let child_slot = Arc::new(Mutex::new(Some(child)));
    let watcher = {
        let slot = Arc::clone(&child_slot);
        let cancel = Arc::clone(&cancel);
        std::thread::spawn(move || loop {
            if cancel.load(Ordering::SeqCst) {
                let mut guard = slot.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(child) = guard.as_mut() {
                    let _ = child.kill();
                }
                break;
            }
            if slot.lock().map_or(true, |guard| guard.is_none()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        })
    };

    let stdout_handle = app.clone();
    let stdout_thread = std::thread::spawn(move || stream_output(stdout, &stdout_handle, "stdout"));
    let stderr_handle = app.clone();
    let stderr_thread = std::thread::spawn(move || stream_output(stderr, &stderr_handle, "stderr"));

    stdout_thread.join().ok();
    stderr_thread.join().ok();

    let status = {
        let mut guard = child_slot.lock().unwrap_or_else(|err| err.into_inner());
        guard
            .take()
            .ok_or_else(|| "校验进程已退出".to_string())?
            .wait()
            .map_err(|err| format!("等待校验进程结束时出错：{err}"))?
    };
    watcher.join().ok();

    if cancel.load(Ordering::SeqCst) {
        return Err("已取消校验".to_string());
    }

    Ok(status.code().unwrap_or(-1))
}

fn stream_output(mut reader: impl Read, app: &tauri::AppHandle, stream: &'static str) {
    let mut chunk = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last: Option<String> = None;

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for &byte in &chunk[..read] {
            match byte {
                b'\n' | b'\r' => emit_line(app, stream, &mut pending, &mut last),
                0x08 => {
                    pending.pop();
                }
                _ if pending.len() < 64 * 1024 => pending.push(byte),
                _ => {}
            }
        }
    }
    emit_line(app, stream, &mut pending, &mut last);
}

fn emit_line(
    app: &tauri::AppHandle,
    stream: &'static str,
    pending: &mut Vec<u8>,
    last: &mut Option<String>,
) {
    if pending.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(pending).trim_end().to_string();
    pending.clear();
    if text.is_empty() || last.as_deref() == Some(text.as_str()) {
        return;
    }
    *last = Some(text.clone());
    let _ = app.emit_to("verify", VERIFY_OUTPUT, VerifyLine { stream, text });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            extracting: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            verify_archive: Mutex::new(None),
            verifying: AtomicBool::new(false),
            verify_cancel: Arc::new(AtomicBool::new(false)),
        })
        .invoke_handler(tauri::generate_handler![
            pick_archive,
            pick_folder,
            start_extract,
            open_verify_window,
            verify_start
        ])
        .on_window_event(|window, event| {
            if window.label() == "verify" {
                if let WindowEvent::CloseRequested { .. } = event {
                    window
                        .app_handle()
                        .state::<AppState>()
                        .verify_cancel
                        .store(true, Ordering::SeqCst);
                }
                return;
            }

            if let WindowEvent::CloseRequested { api, .. } = event {
                let extracting = window
                    .app_handle()
                    .state::<AppState>()
                    .extracting
                    .load(Ordering::SeqCst);
                if !extracting {
                    return;
                }

                api.prevent_close();
                let window = window.clone();
                window
                    .dialog()
                    .message("正在解压，退出会中断当前任务。确定要退出吗？")
                    .title("确认退出")
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::OkCancelCustom(
                        "退出".to_string(),
                        "继续解压".to_string(),
                    ))
                    .show(move |confirmed| {
                        if !confirmed {
                            return;
                        }
                        let handle = window.app_handle().clone();
                        handle
                            .state::<AppState>()
                            .cancel
                            .store(true, Ordering::SeqCst);
                        // Give the running tool a moment to be killed, but never
                        // leave the user stuck if extraction is still scanning.
                        std::thread::spawn(move || {
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_millis(800);
                            while handle.state::<AppState>().extracting.load(Ordering::SeqCst)
                                && std::time::Instant::now() < deadline
                            {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }
                            handle.exit(0);
                        });
                    });
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
