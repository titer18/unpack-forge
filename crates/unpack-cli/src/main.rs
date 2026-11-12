use anyhow::{bail, Result};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;
use unpack_core::{extract, ExtractEvent, ExtractOptions};

const USAGE: &str = "\
unpack — 解压分卷 7z / rar 压缩包

用法:
  unpack <archive> -o <输出目录> [选项]

选项:
  -o, --output <dir>   输出目录（必填）
  --delete-after       解压成功后删除原分卷文件
  --tools-dir <dir>    7zz / unrar 所在目录（默认：同目录或 PATH）
  --json               输出机器可读的进度（每行一个 JSON 事件到 stdout）
  -h, --help           显示帮助
";

#[derive(Debug, Clone, Copy)]
enum Mode {
    Human,
    Json,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }

    let mut archive: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut tools_dir: Option<PathBuf> = None;
    let mut delete_after = false;
    let mut mode = Mode::Human;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                i += 1;
                output = Some(PathBuf::from(args.get(i).map(String::as_str).unwrap_or("")));
            }
            "--delete-after" => delete_after = true,
            "--tools-dir" => {
                i += 1;
                tools_dir = Some(PathBuf::from(args.get(i).map(String::as_str).unwrap_or("")));
            }
            "--json" => mode = Mode::Json,
            _ if !args[i].starts_with('-') && archive.is_none() => {
                archive = Some(PathBuf::from(&args[i]));
            }
            other => bail!("未知参数：{other}\n\n{USAGE}"),
        }
        i += 1;
    }

    let archive = archive.ok_or_else(|| anyhow::anyhow!("缺少压缩包路径\n\n{USAGE}"))?;
    let output = output.unwrap_or_else(|| archive.with_extension(""));

    let start = Instant::now();
    let options = ExtractOptions {
        archive,
        out_dir: output,
        tools_dir,
        delete_after,
        cancel: None,
    };

    extract(options, |event| match mode {
        Mode::Human => print_human(event, start),
        Mode::Json => print_json(event),
    })?;

    Ok(())
}

fn print_human(event: ExtractEvent, start: Instant) {
    if let Some(message) = &event.message {
        println!("{message}");
        return;
    }

    let speed = event
        .speed_bps
        .map(format_speed)
        .unwrap_or_else(|| "…".to_string());
    let eta = event
        .eta_secs
        .map(|s| format!("ETA {}", format_duration(s)))
        .unwrap_or_else(|| "ETA …".to_string());

    eprint!(
        "\r{:>5.1}%  速度 {}  {}  已用 {}",
        event.percent,
        speed,
        eta,
        format_duration(start.elapsed().as_secs())
    );
    let _ = std::io::stderr().flush();
    if event.percent >= 100.0 {
        eprintln!();
    }
}

fn print_json(event: ExtractEvent) {
    let line = serde_json_progress(event);
    println!("{line}");
    let _ = std::io::stdout().flush();
}

fn serde_json_progress(event: ExtractEvent) -> String {
    // A tiny hand-rolled JSON encoder keeps the CLI dependency-free.
    format!(
        "{{\"percent\":{:.1},\"bytes\":{},\"total_bytes\":{},\"speed_bps\":{},\"eta_secs\":{},\"message\":{}}}",
        event.percent,
        opt_u64(event.bytes),
        opt_u64(event.total_bytes),
        opt_f64(event.speed_bps),
        opt_u64(event.eta_secs),
        json_string(event.message.as_deref())
    )
}

fn opt_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

fn opt_f64(v: Option<f64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

fn json_string(v: Option<&str>) -> String {
    match v {
        Some(s) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
        None => "null".to_string(),
    }
}

fn format_speed(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes_per_sec;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!(
            "{}h{:02}m{:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
