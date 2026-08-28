# AirFerry

**English** | [简体中文](docs/README.zh-CN.md)

> Fully Offline Optical File Transfer

Transfer files through **screen-displayed QR video streams and camera scanning**—without the Internet, a local network, Bluetooth, USB, NFC, or any other communication channel. Built for air-gapped environments.

> 🤖 **AI agents / new developers**: Start with [AGENTS.md](AGENTS.md) for build commands, code navigation, debugging references, and known documentation/code discrepancies. The authoritative bit-level cross-platform wire-format specification is [docs/SPEC.md](docs/SPEC.md).

- **Senders**: Browser extensions for Chrome, Edge, and Firefox (MV2 and MV3) · **Web sender** ([online version](#web-sender--receiver))
- **Receivers**: Native Android app · Windows desktop app (WPF) · **Web receiver** ([online version](#web-sender--receiver))
- **Core library**: Rust, compiled to **WebAssembly** for browser clients, an **Android native library** via JNI, and a **Windows DLL** via C ABI/P/Invoke, ensuring identical codec behavior across platforms

## Data Flow

```text
Sender                                           Receiver
File                                             Camera video stream (locked at ~60 fps)
  │ Best of Raw / Zstd / XZ compression            │
  ├─ Chunking                                       │
  ├─ RaptorQ encoding (RFC 6330)                    │
  ├─ QR frame generation (one source pass → fresh repair) ── video stream ──► Parallel QR decoding (N×ZXing-C++)
  └─ Continuous playback (15/20/30/45/60/90/120 fps or unlimited; default 60) ├─ Serial RaptorQ ingestion/recovery
                                                    ├─ Decompression
                                                    ├─ File reassembly
                                                    └─ File saving
```

## Features

- ✅ High reliability and fault tolerance under heavy frame loss, reordering, duplicate frames, and partial corruption
- ✅ Segmented large-file transfer: compress first, then split the compressed stream into ~32 MiB segments; supports files, multi-file bundles, and text
- ✅ Continuously fresh fountain coding: source symbols are sent once, followed by non-repeating repair symbols; progress is approximately linear, and transmission stops explicitly at the RFC 24-bit ESI limit
- ✅ Parallel receiver decode pool: multithreaded ZXing with serial native ingestion makes full use of high-frame-rate capture
- ✅ Resumable large-file transfers: the history page identifies missing segments, while verified segments survive restarts
- ✅ Continuous QR video streams at 15 / 20 / 30 / 45 / 60 / 90 / 120 fps or unlimited speed (default: 60 fps)
- ✅ Zero network dependency for air-gapped environments
- ✅ One-way channel with no acknowledgements required
- ✅ Automatic compression selection among Raw, Zstd Lv1, and Xz Lv9, choosing the smallest result
- ✅ Multi-file transfer: two or more items are automatically packed into one ETBUNDL1 container and sent through the same QR stream
- ✅ Mixed file and text transfers: one unified selection list, full-page file/folder drag-and-drop, and ETTEXTv1 for a single plain-text item that receivers can copy
- ✅ Receiver-side copy, share, and save support for text files such as txt, md, json, and source code
- ✅ Four-code parallel mode: each frame tiles four different symbols for ~4× throughput; enabled by default
- ✅ Speed presets: Stable / Fast / Extreme / Aggressive / Maximum / Extreme 2400B; default is Aggressive at 1400B@60fps
- ✅ Chrome, Edge, and Firefox support across MV2 and MV3
- ✅ Web, Android, and Windows receivers share the same Rust protocol core; Windows supports cameras, USB/HDMI/SDI capture cards, and screen-region or individual-window capture for camera-free same-machine, VM, and remote-desktop workflows

## Web Sender & Receiver

No installation is required. Open either client directly in a browser; GitHub Pages builds and deploys them automatically.

| Client | URL | Purpose |
|--------|-----|---------|
| **Web sender** | <https://UR-SillyB.github.io/AirFerry/> | Plays a QR video stream in the browser to send files |
| **Web receiver** | <https://UR-SillyB.github.io/AirFerry/receiver/> | Scans QR codes with a camera to recover files |

> ⚠️ The **web receiver** must run over **HTTPS or localhost** to access the camera due to browser security requirements. GitHub Pages uses HTTPS and works directly. Browser camera pipelines and JS/WASM decoding make the web receiver slower than native clients; for maximum speed and reliable large-file recovery, use the native Android or Windows receiver below.

## Downloads

The latest release is [GitHub Release v1.2.8](https://github.com/UR-SillyB/AirFerry/releases/tag/v1.2.8).

| File | Description |
|------|-------------|
| `airferry-sender-chrome-mv3-v1.2.8.crx` / `.zip` | Modern scalar Chrome / Edge MV3 build; the CRX is signed with the fixed release key; use the zip if browser policy blocks the CRX |
| `airferry-sender-chrome-mv2-v1.2.8.crx` / `.zip` | Legacy-compatible scalar Chrome / Edge MV2 build; the CRX uses the same fixed release key |
| `airferry-sender-firefox-mv3-v1.2.8.xpi` | Firefox MV3 extension for Firefox 116+ |
| `airferry-sender-firefox-mv2-v1.2.8.xpi` | MV2-compatible extension for Firefox 91+ |
| `airferry-sender-web-v1.2.8.zip` | Static web sender with modern scalar WASM; deploy it to any static host (official [online version](#web-sender--receiver)) |
| `airferry-sender-web-standalone-v1.2.8.html` | Standalone web sender, about 2 MB; double-click to use with no server required |
| `airferry-receiver-web-v1.2.8.zip` | **Web receiver**; deploy to HTTPS or localhost before using the camera (official [online version](#web-sender--receiver)) |
| `airferry-receiver-android-arm64-v1.2.8.apk` | **Android receiver** for arm64-v8a and Android 10+; signed with the fixed release keystore |
| `airferry-receiver-windows-x64-v1.2.8.zip` | **Windows receiver** for x64 and Windows 10+; supports cameras, USB/HDMI/SDI capture cards, and screen-region/window capture |

> Sender, APK, and web artifacts are produced by `./scripts/build-all.sh release`; the version is read from `apps/sender/package.json`. The Windows zip is normally uploaded to the same release by the GitHub Actions `windows` workflow through `workflow_dispatch`. Chrome `.crx` signing requires Chrome on the build machine; otherwise only `.zip` is produced. The GitHub Actions `pages` workflow automatically builds and deploys both web clients to GitHub Pages on pushes to `main`.

### Android Receiver

Download the APK, allow installation from unknown sources, and install it on an Android 10+ device. The APK is signed with the release keystore.

### Windows Receiver

Extract `airferry-receiver-windows-x64-v1.2.8.zip`, install the [.NET 8 Desktop Runtime](https://dotnet.microsoft.com/download/dotnet/8.0), and run `AirFerry.exe`. At startup, choose a camera, capture card, or screen capture from the unified, mutually exclusive scan-source list; USB/HDMI/SDI capture cards are labeled automatically. Then press the primary button to begin.

Choosing **Screen capture** opens a screenshot-style selector. Drag to select a **screen region**, click to select a **window** (hovering highlights it), or **right-click to select the entire screen**. Full-screen capture is preferred for full-screen apps and games: borderless games may minimize when focus changes, while exclusive full-screen windows cannot be captured as individual windows. This mode is useful for end-to-end tests with a browser playing QR codes on the same machine and for camera-free VM or remote-desktop windows. Press Esc to cancel. On the scan page, point the selected source at the on-screen QR codes.

### Chrome / Edge Extensions

1. Prefer the matching `.crx`: MV3 is the modern scalar build, while MV2 supports older browsers. If browser policy blocks externally distributed CRX installation, download and extract the matching `.zip` instead.
2. For a zip, open `chrome://extensions` and enable **Developer mode** in the upper-right corner.
3. Click **Load unpacked** and select the extracted directory.

> The v1.2.8 CRX files reuse the original fixed private key, so both MV2 and MV3 retain the extension ID `lgafjpalpcbiellnlbfdabdlbfooojjm`. The zip is the fallback when the browser blocks an externally distributed CRX.

### Firefox Extension

> The published `.xpi` files are **not signed by Mozilla**. Mozilla does not support purely local signing; signing must go through AMO. Regular Firefox releases will therefore reject them. Available options:
>
> - On **Developer Edition, Nightly, or ESR**, set `xpinstall.signatures.required` to `false` in `about:config`, then follow the steps below.
> - Extract the `.xpi`, then load it temporarily from `about:debugging#/runtime/this-firefox` → **Load Temporary Add-on**. It will be removed after a restart.
> - Upload the `.xpi` to [addons.mozilla.org](https://addons.mozilla.org/developers/) for AMO server-side signing and distribution, which is recommended for formal releases.

1. Download the matching `.xpi` file: MV3 requires Firefox 116+, while MV2 requires Firefox 91+.
2. Open `about:addons` → gear icon → **Install Add-on From File**, then select the `.xpi`.
3. Alternatively, use **Load Temporary Add-on** in `about:debugging#/runtime/this-firefox`.

## Repository Structure

```text
AirFerry/
├── core/                  # Cross-platform Rust protocol core + Windows ZXing-C++ camera-decoding core
│   ├── raptorq-core/      # RFC 6330 RaptorQ codec wrapper
│   ├── qr-protocol/       # Frame format / chunking / compression / CRC / QR matrix
│   ├── transfer-engine/   # Orchestration / state machine / progress / resume + WASM/JNI/C ABI
│   └── zxing-decoder/     # Windows ZXing-C++ implementation matching the Android v1.1.3 mode
├── apps/
│   ├── sender/            # Plasmo + React + TS + WASM sender (browser extension)
│   ├── scanner/           # Kotlin + CameraX + ZXing-C++ receiver (Android app)
│   └── windows/           # C# WPF + OpenCvSharp + ZXing-C++ (Windows app)
├── scripts/
│   ├── build-all.sh       # One-command build and packaging, including crx/xpi signing and the windows subcommand
│   └── build-windows.ps1  # Native Windows PowerShell build script (preferred)
├── docs/                  # Protocol, architecture, API, and build documentation in Chinese
├── Cargo.toml             # Rust workspace root configuration
└── .gitignore             # dist/ artifacts are not committed; distribution uses GitHub Releases
```

## Quick Start

See [Development Setup](docs/dev-setup.md). Build commands by component:

| Component | Command | Notes |
|-----------|---------|-------|
| Core library | `cargo build` / `cargo test` | Rust workspace |
| Browser extensions | `npm run build` | Builds all four extension targets |
| Android app | `./gradlew assembleDebug` | Requires the Android NDK |
| Windows app | `./scripts/build-windows.ps1` | Requires Windows, the .NET 8 SDK, and CMake/Visual Studio C++; see the [Windows build guide](docs/build-windows.md) |

## Technical Architecture

- **Coding layer**: RaptorQ fountain codes (RFC 6330). After sending each source symbol once, the sender continuously emits fresh repair symbols with monotonically increasing, non-repeating ESIs up to 2²⁴−1. Receivers may join at any time.
- **Bundling layer**: Two or more files are packed into an ETBUNDL1 container, then sent through one compression stream and one RaptorQ stream.
- **Compression layer**: The smallest result is selected among Raw, Zstd Lv1, and Xz Lv9. A 70% Zstd early-exit heuristic skips the slower Xz pass.
- **Transport layer**: A 60-byte frame header, a `symbol_size` payload (1400 bytes by default in the browser), and a 4-byte CRC are encoded into the **smallest possible** EC-L QR code. A **1464-byte frame fits QR version 27 at 125×125 modules**. Four-code mode tiles four symbols per frame for ~4× throughput.
- **Protocol layer**: Descriptor frames appear every 17 frames, with the very first frame being a descriptor. They carry OTI and file metadata: filename, size, CRC32, and compression tag. Because 17 is coprime with the two- and four-code layouts, descriptors rotate through every on-screen code position.
- **Receiver layer**: Android uses CameraX with the fixed v1.1.3 Kotlin scheduler and JNI ZXing-C++ decoding path. Windows uses OpenCvSharp DirectShow and mirrors the same v1.1.3 full-frame/ROI mode through `core/zxing-decoder/`. Both use 2–6 workers, four-symbol batch ingestion, and serial Rust ingestion. Windows copies grayscale frames only once through buffer pooling; its UI displays a three-second rolling rate and effective throughput at about 7 Hz.

## Documentation

- [AGENTS.md](AGENTS.md) — AI agent operations manual: build commands, code navigation, debugging references, and known discrepancies
- [Protocol specification](docs/protocol.md) — complete protocol description
- [Cross-platform contract specification](docs/SPEC.md) — authoritative bit-level wire format, session ID, and JNI layout
- [QR frame format](docs/qr-frame-format.md) — frame-header field definitions
- [RaptorQ parameters](docs/raptorq-params.md) — codec parameter details
- [Architecture](docs/architecture.md) — system architecture and component relationships
- [Data flow](docs/data-flow.md) — end-to-end data flow
- [API reference](docs/api.md) — core API documentation
- [Browser extension build guide](docs/build-browser.md)
- [Android build guide](docs/build-android.md)
- [Windows build guide](docs/build-windows.md)
- [Development setup](docs/dev-setup.md)

## Acknowledgements

- [RaptorQR](https://github.com/infrost/RaptorQR) (MIT, © 2026 Haixiang) — an offline optical-transfer tool also built around a Rust-to-WASM RaptorQ fountain-code pipeline and parallel QR playback. AirFerry draws on its pioneering exploration of the “Rust core compiled to WASM + browser QR video stream” architecture.
- [cberner/raptorq](https://github.com/cberner/raptorq) — the RFC 6330 RaptorQ implementation in Rust used as a core dependency.

## Related Links

- [linux.do](https://linux.do) — a sincere, friendly, and practical open-source technology community

## License

MIT
