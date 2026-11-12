# Unpack Forge

基于 [Tauri 2](https://v2.tauri.app/) 的 macOS 分卷压缩包解压工具，同时提供图形界面（GUI）和命令行工具（CLI）。

支持 `7z` / `Zip` / `RAR` 分卷压缩包，例如 PS5 游戏转储常见的大体积多卷归档。

## 功能

- 支持分卷 `7z`（`archive.7z.001` …）、`Zip`（`archive.zip.001` / `archive.z01` …）和 `RAR`（`archive.part1.rar` / `archive.part01.rar` / `archive.rar` …）。
- GUI 选择压缩包、选择输出目录、勾选“解压成功后删除原文件”，点击开始即可解压。
- 勾选删除后，成功解压会删除检测到的**整组分卷**，而不只是用户选中的第一个分卷。
- 实时进度条，显示解压速度、预计剩余时间和已解压大小。
- 总大小按整个分卷集统计；`unrar` 进度按输出文件实际写入字节计算，不依赖其被缓冲的百分比输出。
- 开始前检查目标盘剩余空间，空间不足时直接提示。
- “校验”按钮打开独立窗口，滚动输出 `7zz t -bb1` / `unrar t` 的逐文件结果，可复制全部日志。
- 解压中关闭窗口会二次确认，避免误关闭中断任务。
- 独立 CLI，支持人类可读进度和 `--json` 机器可读进度。

## 下载

在 [Releases](https://github.com/titer18/unpack-forge/releases) 页面下载最新版本的 macOS（Apple Silicon / arm64）构建产物：

- `Unpack-Forge-macos-arm64.zip`：图形界面 App。
- `unpack-cli-macos-arm64.zip`：命令行工具，内含 `unpack`、`7zz`、`unrar`。

> 当前发布的二进制面向 Apple Silicon Mac。项目中的 `7zz` 与 `unrar` sidecar 也是 `aarch64-apple-darwin`。

## 使用

### GUI

1. 解压 `Unpack-Forge-macos-arm64.zip`，将 `Unpack Forge.app` 拖到 `应用程序`。
2. 首次打开若被 macOS 拦截，见下方 [macOS 自签名与首次打开](#macos-自签名与首次打开)。
3. 点击“选择压缩包”，选择任意一个分卷（例如 `.part01.rar` 或 `.7z.001`）。
4. 点击“选择保存目录”，选择输出位置。
5. 如需解压成功后删除所有原分卷，勾选“解压成功后删除原文件”。
6. 点击“开始”，等待进度完成。

### CLI

```bash
unpack <archive> -o <输出目录> [选项]

  -o, --output <dir>   输出目录（必填）
  --delete-after       解压成功后删除原分卷文件
  --tools-dir <dir>    7zz / unrar 所在目录
  --json               输出 JSON 进度（每行一个事件）
  -h, --help           显示帮助
```

示例：

```bash
unpack game.7z.001 -o /Volumes/Game --delete-after
unpack movie.part1.rar -o ~/Movies --json
```

## 目录结构

```text
.
├── app/                      # Tauri 2 前端（React + TypeScript + Vite）
│   ├── src/                  # 界面代码
│   └── src-tauri/            # Rust 后端与打包配置
│       └── binaries/         # 7zz / unrar sidecar（按目标三元组命名）
├── crates/
│   ├── unpack-core/          # 共享解压核心（分卷识别、外部工具调用、进度解析）
│   └── unpack-cli/           # 命令行工具（二进制名：unpack）
├── tools/                    # 与 CLI 一起分发的 7zz / unrar
└── .github/workflows/        # Tag 自动打包发布
```

## 从源码构建

需要以下环境：

- macOS（Apple Silicon）
- [Rust](https://rustup.rs/) stable
- Node.js 22+

```bash
git clone https://github.com/titer18/unpack-forge.git
cd unpack-forge

cd app
npm ci
cd ..

# 构建 CLI
cargo build -p unpack-cli --release

# 构建 GUI .app
cd app
npx tauri build
```

GUI 产物位于：

```text
app/src-tauri/target/release/bundle/macos/Unpack Forge.app
```

如果 npm 安装卡在 `node-gyp`，可改用：

```bash
npm ci --ignore-scripts
```

## Tag 自动打包

项目包含 `.github/workflows/release.yml`，推送 `v*` 标签时会自动触发 macOS（arm64）构建，并创建 GitHub Release：

```bash
git tag v0.1.0
git push origin v0.1.0
```

Action 会自动打包两个产物：

- `Unpack-Forge-macos-arm64.zip`
- `unpack-cli-macos-arm64.zip`

推送新的 `v*` tag 即可触发；首次运行会在 Actions 页面显示 `release` workflow。

## macOS 自签名与首次打开

Release 构建使用本地 ad-hoc 签名（未使用 Apple Developer ID），macOS 可能提示“无法验证开发者”。

### 首次打开

任选一种方式：

- 在 Finder 中右键 `Unpack Forge.app` → 打开 → 再点一次“打开”。
- 或进入 **系统设置 → 隐私与安全性**，在“安全性”区域点击“仍要打开”。

如果 App 从网络下载后带有隔离属性，可先移除：

```bash
xattr -cr "/Applications/Unpack Forge.app"
```

### 复制后重新 ad-hoc 签名

把 `.app` 复制到其他目录后，建议重新 ad-hoc 签名，避免旧的 `_CodeSignature` 与文件不匹配：

```bash
codesign --force --deep --sign - "/Applications/Unpack Forge.app"
codesign --verify --deep --strict "/Applications/Unpack Forge.app"
```

复制时可使用 `ditto`，减少 Finder 元数据残留：

```bash
ditto --noextattr --norsrc "Unpack Forge.app" "/Applications/Unpack Forge.app"
xattr -d com.apple.FinderInfo "/Applications/Unpack Forge.app" 2>/dev/null || true
codesign --force --deep --sign - "/Applications/Unpack Forge.app"
codesign --verify --deep --strict "/Applications/Unpack Forge.app"
```

> ad-hoc 签名不会通过 Apple 公证。若需要彻底消除 Gatekeeper 警告，应使用 Apple Developer ID 证书签名并公证。

## 外部工具与许可

- `7zz`：7-Zip (z)，用于 `.7z` 与 `.7z.001` 分卷。
- `7zz` 同时处理 `Zip` 和普通 `RAR`；只有 WinRAR 7 的新格式会回退到 `unrar`。
- `unrar`：RARLAB 提供的官方免费 unrar，用于 `.rar` 分卷。

路由规则：

```text
7z / zip / 普通 RAR → 7zz
WinRAR 7 新格式       → unrar
```

当 `7zz` 能列出 RAR，但解压报 `Unsupported Method` 时，会自动回退到 `unrar` 重试。解压时 `unrar` 使用 `x -o+ -y`（自动覆盖）。

工具查找顺序：

```text
UNPACK_TOOLS_DIR → --tools-dir → 程序同目录 → PATH
```

GUI 打包时会把 `app/src-tauri/binaries/` 下的二进制作为 sidecar 放进 `.app/Contents/MacOS/`。

> `7zz` 和 `unrar` 分属各自上游项目，重新分发前请确认其许可条款允许你的使用方式。

## License

[MIT](LICENSE)
