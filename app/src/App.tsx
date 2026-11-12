import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { ProgressPayload } from "./types";

type Phase = "idle" | "running" | "done" | "error";

const EMPTY_PROGRESS: ProgressPayload = {
  percent: 0,
  bytes: null,
  total_bytes: null,
  speed_bps: null,
  eta_secs: null,
  message: null,
  done: false,
  error: null,
};

function formatBytes(bytes: number | null): string {
  if (bytes == null) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function formatSpeed(bytesPerSec: number | null): string {
  if (bytesPerSec == null) return "—";
  return `${formatBytes(bytesPerSec)}/s`;
}

function formatEta(seconds: number | null): string {
  if (seconds == null) return "—";
  const s = Math.max(0, Math.round(seconds));
  if (s >= 3600) {
    return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m ${s % 60}s`;
  }
  if (s >= 60) {
    return `${Math.floor(s / 60)}m ${s % 60}s`;
  }
  return `${s}s`;
}

export default function App() {
  const [archive, setArchive] = useState("");
  const [outDir, setOutDir] = useState("");
  const [deleteAfter, setDeleteAfter] = useState(false);
  const [phase, setPhase] = useState<Phase>("idle");
  const [progress, setProgress] = useState<ProgressPayload>(EMPTY_PROGRESS);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let cancelled = false;

    listen<ProgressPayload>("unpack://progress", (event) => {
      if (cancelled) return;
      const payload = event.payload;
      setProgress(payload);
      if (payload.done) {
        setPhase(payload.error ? "error" : "done");
        setError(payload.error ?? null);
      } else {
        setPhase("running");
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const pickArchive = useCallback(async () => {
    const value = await invoke<string | null>("pick_archive");
    if (value) {
      setArchive(value);
      setNotice(null);
    }
  }, []);

  const pickFolder = useCallback(async () => {
    const value = await invoke<string | null>("pick_folder");
    if (value) setOutDir(value);
  }, []);

  const start = useCallback(async () => {
    if (!archive || !outDir) return;
    setError(null);
    setProgress(EMPTY_PROGRESS);
    setPhase("running");
    try {
      await invoke("start_extract", {
        archive,
        outDir,
        deleteAfter,
      });
    } catch (err) {
      setPhase("error");
      setError(String(err));
    }
  }, [archive, outDir, deleteAfter]);

  const verify = useCallback(async () => {
    if (!archive) return;
    setNotice(null);
    try {
      await invoke("open_verify_window", { archive });
    } catch (err) {
      setNotice(String(err));
    }
  }, [archive]);

  const running = phase === "running";
  const percent = Math.min(100, Math.max(0, progress.percent));
  const canStart = archive.length > 0 && outDir.length > 0 && !running;

  return (
    <main className="app">
      <header className="header">
        <h1>Unpack Forge</h1>
        <p>分卷 7z / RAR 解压工具 · Tauri 2</p>
      </header>

      <section className="field">
        <label htmlFor="archive">压缩包（选择第一个分卷）</label>
        <div className="row">
          <input
            id="archive"
            value={archive}
            onChange={(e) => setArchive(e.target.value)}
            placeholder="例如 archive.7z.001 或 archive.part1.rar"
            spellCheck={false}
          />
          <button type="button" onClick={pickArchive} disabled={running}>
            浏览…
          </button>
          <button type="button" onClick={verify} disabled={!archive || running}>
            校验
          </button>
        </div>
        {notice && <div className="hint">{notice}</div>}
      </section>

      <section className="field">
        <label htmlFor="outDir">保存到</label>
        <div className="row">
          <input
            id="outDir"
            value={outDir}
            onChange={(e) => setOutDir(e.target.value)}
            placeholder="选择输出目录"
            spellCheck={false}
          />
          <button type="button" onClick={pickFolder} disabled={running}>
            浏览…
          </button>
        </div>
      </section>

      <label className="checkbox">
        <input
          type="checkbox"
          checked={deleteAfter}
          onChange={(e) => setDeleteAfter(e.target.checked)}
          disabled={running}
        />
        <span>解压成功后删除原分卷文件</span>
      </label>

      <button
        className="start"
        type="button"
        onClick={start}
        disabled={!canStart}
      >
        {running ? "解压中…" : "开始解压"}
      </button>

      <section className="progress-panel">
        <div className="progress-track" aria-hidden="true">
          <div className="progress-fill" style={{ width: `${percent}%` }} />
        </div>
        <div className="progress-text">{percent.toFixed(1)}%</div>

        <div className="stats">
          <div>
            <span className="stat-label">速度</span>
            <span className="stat-value">{formatSpeed(progress.speed_bps)}</span>
          </div>
          <div>
            <span className="stat-label">剩余时间</span>
            <span className="stat-value">{formatEta(progress.eta_secs)}</span>
          </div>
          <div>
            <span className="stat-label">已解压</span>
            <span className="stat-value">
              {formatBytes(progress.bytes)} / {formatBytes(progress.total_bytes)}
            </span>
          </div>
        </div>

        {(progress.message || error) && (
          <div className={`message ${phase === "error" ? "error" : ""}`}>
            {error ?? progress.message}
          </div>
        )}
      </section>
    </main>
  );
}
