import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

type VerifyStatus = "running" | "ok" | "fail";

type VerifyLine = {
  stream: string;
  text: string;
};

type VerifyDone = {
  ok: boolean;
  code: number | null;
  error: string | null;
};

const VERIFY_OUTPUT = "verify://output";
const VERIFY_DONE = "verify://done";
const VERIFY_BEGIN = "verify://begin";

export default function VerifyView() {
  const [archive, setArchive] = useState("");
  const [lines, setLines] = useState<string[]>([]);
  const [status, setStatus] = useState<VerifyStatus>("running");
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const logRef = useRef<HTMLPreElement>(null);
  const stickToBottom = useRef(true);

  const start = useCallback((archiveHint?: string) => {
    setLines([]);
    setError(null);
    setStatus("running");
    stickToBottom.current = true;
    if (archiveHint) setArchive(archiveHint);

    invoke<string>("verify_start")
      .then((path) => setArchive(path))
      .catch((err) => {
        const message = String(err);
        if (message.includes("正在进行")) return;
        setError(message);
        setStatus("fail");
      });
  }, []);

  useEffect(() => {
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];

    (async () => {
      const offOutput = await listen<VerifyLine>(VERIFY_OUTPUT, (event) => {
        const { stream, text } = event.payload;
        setLines((prev) => [...prev, stream === "stderr" ? `[stderr] ${text}` : text]);
      });
      const offDone = await listen<VerifyDone>(VERIFY_DONE, (event) => {
        const { ok, code, error: failure } = event.payload;
        if (failure) {
          setError(failure);
          setStatus("fail");
          setLines((prev) => [...prev, "", `[结束] ${failure}`]);
          return;
        }
        setStatus(ok ? "ok" : "fail");
        setLines((prev) => [
          ...prev,
          "",
          `[结束] 退出码 ${code ?? "?"} · ${ok ? "校验通过" : "校验失败"}`,
        ]);
      });
      const offBegin = await listen<string>(VERIFY_BEGIN, (event) => {
        start(event.payload);
      });
      unlisteners.push(offOutput, offDone, offBegin);

      if (disposed) {
        unlisteners.forEach((unlisten) => unlisten());
        return;
      }
      start();
    })();

    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [start]);

  useEffect(() => {
    const element = logRef.current;
    if (element && stickToBottom.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [lines]);

  const copyAll = useCallback(async () => {
    const text = lines.join("\n");
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const textarea = document.createElement("textarea");
      textarea.value = text;
      textarea.style.position = "fixed";
      textarea.style.opacity = "0";
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand("copy");
      document.body.removeChild(textarea);
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  }, [lines]);

  const label =
    status === "running" ? "校验中…" : status === "ok" ? "校验通过" : "校验失败";

  return (
    <main className="verify">
      <header className="verify-header">
        <div className="verify-title">
          <h1>校验结果</h1>
          <p title={archive}>{archive || "…"}</p>
        </div>
        <div className="verify-actions">
          <span className={`status-pill ${status}`}>{label}</span>
          <button type="button" onClick={copyAll} disabled={lines.length === 0}>
            {copied ? "已复制" : "复制全部"}
          </button>
        </div>
      </header>
      {error && <div className="verify-error">{error}</div>}
      <pre
        className="verify-log"
        ref={logRef}
        tabIndex={0}
        onScroll={(event) => {
          const element = event.currentTarget;
          stickToBottom.current =
            element.scrollHeight - element.scrollTop - element.clientHeight < 24;
        }}
      >
        {lines.join("\n")}
      </pre>
    </main>
  );
}
