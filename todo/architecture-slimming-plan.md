# AirFerry 架构瘦身与工程优化计划（v3 终版）

> 状态：待执行
> 日期：2026-08-16
> 来源：代码量/复杂度分析 → 两轮评审修正后的终版
> 分支：`plan/architecture-slimming`

## 背景与总体原则

当前复杂度的主要来源不是 Rust 编解码核心，而是：

1. 同一协议语义在 Android、Windows、Web 各实现一次；
2. JNI、C ABI、WASM 三套绑定重复暴露大量 getter；
3. 浏览器端为 MV2/MV3、双 WASM、双 QR 后端维护了较大的构建矩阵；
4. 三个接收端控制器都已接近 1,700~1,900 行，承担过多职责；
5. 文档、版本号、配置存在多份事实源。

**核心原则：每一步都可独立发布、可回滚**（仓库约 4 天一版，不允许长活分支）。

## 已确认的决策

| 决策点 | 结论 |
|---|---|
| FAST-only 本地开发策略 | **硬性要求 emsdk**：彻底删除 `zxing-wasm` 双后端，dev 与 release 构建均强制 FAST 产物，缺失即失败 |
| 控制器拆解范围 | **三端全部拆解**：Windows 试点 → Android → Web |
| SIMD 双 WASM | 移除。统一定死 `wasm-bindgen = 0.2.92` 标量单产物 |
| ZXing 双兼容 | 移除。全链路收敛为 FAST ZXing-C++ |
| MV2 / Chrome 87 兼容 | 保留。单份标量 WASM 通吃 Chrome 87 → 最新版 |

## 目标指标

| 指标 | 现状 | 目标 |
|---|---|---|
| 手写生产代码 LOC | ~41,000 | 净减 2,800~4,600 |
| FFI 导出函数 | 每端 15+ getter | 每端 5~6 个核心接口 |
| 协议解析实现份数 | 3 份（Kt/C#/TS） | 1 份（Rust） |
| package/lockfile 数量 | 2 package + 3 lock | 1 package + 1 lock |
| 文档重复事实 | 13 篇根文档 ~2,940 行 | 4 核心文档，减 700~1,100 行 |

---

## 执行序列（10 步，按序落地）

### 步骤 1：基线锁死（前置，不改产品代码）

- **Golden vectors 落地为机制**而非概念：
  - 建 `core/testdata/` 提交共享 fixture + Rust 生成器脚本；
  - 覆盖 descriptor v1/v3/v5、压缩 none/zstd/xz、单文件/ETTEXT/ETTEXT 超限/ETBUNDL、分段乱序/重复/缺失、CRC32=0、非法 UTF-8、恶意 bundle 长度、snapshot JSON golden 样本；
  - **三端测试断言同一份 fixture**——这是跨端一致性的实际保障。
- `node scripts/version.mjs check`：以根 `Cargo.toml [workspace.package].version` 为事实源，校验全部版本位点命中次数精确，失败即退出。CI 门禁接入。

### 步骤 2：删除死代码（-750~850 行）

- 删除：
  - `core/transfer-engine/src/assembler.rs`（纯内存组装器，产品端走各端持久化实现）
  - `core/transfer-engine/src/resume.rs`（未暴露给任何 FFI 的断点序列化）
  - `Error::InvalidResume`、`ReceiverSession::save_state/restore`
  - 先确认 serde feature 无其他消费者再动依赖。
- `tests/segment_e2e.rs` **不删行为覆盖**：改用测试内部几十行局部拼接辅助器，继续覆盖：
  - descriptor-v5 校验；
  - 段 offset/count/child session ID；
  - 乱序分段最终拼接；
  - root SHA-256；
  - 压缩后分段、拼接后只解压一次。
- 测试代码不计入减行目标。

### 步骤 3：快照化 FFI（增量三段式，-500~750 行）

**迁移顺序（先增后删，禁止一次性切换）：**

1. **增量加入**：三端新增 `receiver_snapshot_json`（老 getter 全保留），在 ingest 排他锁下生成，杜绝跨 getter 状态撕裂；
2. **逐端迁移**：Android → Windows → Web 各一提交，各自过门禁；
3. **收尾删除**：移除 15+ 旧 getter；JNI `AIRFERRY_NATIVE_ABI_VERSION = 2`；C-ABI 新增 `airferry_native_abi_version()`，Windows `NativeBridge.cs` 启动时调用并校验（镜像 Android 现有握手）。

**快照契约细节：**

```rust
struct ReceiverSnapshotV1 {
    schema_version: u32,           // = 1
    meta_confirmed: bool,
    file_name: String,
    original_size: u64,            // JSON number（协议上界 ~4.2 PB < 2^53，schema 注明上界）
    compressed_size: u64,
    compressed_size_known: bool,
    compression: u8,
    crc32: u32,
    crc32_known: bool,
    session_id_hex: String,        // 32-char hex（128 位随机值，真实精度风险点）
    segmented: bool,
    segment_index: u32,
    segment_count: u32,
    root_original_size: u64,
    original_offset: u64,
    root_session_id_hex: String,
    raw_sha256_hex: String,        // hex，避免三端处理 byte array
    root_sha256_hex: String,
}
```

- 保留 `compressed_size_known` / `crc32_known`；
- `progress_json` 独立不合并（刷新频率高于描述符快照）；
- C-ABI 内存所有权：返回 Rust 分配的 NUL 结尾 UTF-8 `char*`，配 `airferry_free_string` 释放（沿用仓库既有模式）；
- 三端测试断言步骤 1 的 golden snapshot JSON。

### 步骤 4：WASM 单轨化（-150~250 行）

- 统一 `wasm-bindgen = 0.2.92` 标量单产物，Chrome 87 → 最新版通吃；
- 扩展是**纯发送端**，FAST ZXing 的 `-msimd128` 不影响扩展兼容性（FAST 只进 web 接收端产物）；
- **原子发布用 symlink 切换**（`ln -sfn` 新 stage 目录 + 删旧），不是目录 rename——POSIX rename 到非空目录会失败；不引入复杂 owner 锁；
- **同步修改耦合文件清单**（漏一即断 CI）：
  - `apps/web/scripts/prepare-wasm.cjs`（现校验 `wasm-pkg-simd/`）
  - `apps/sender/scripts/build-all.cjs` 的 `useWasmPkg()` 双目录切换
  - `.github/workflows/pages.yml`（跑 `npm run wasm`）
  - `.github/workflows/windows.yml`（读 `apps/sender/package.json` 版本路径）
  - `scripts/build-fastzxing.sh` 输出路径
- 构建 target 按 mode 分层：
  - 扩展 mode：`["chrome87", "firefox91"]`（MV3 下限由 manifest 版本决定为 Chrome 88）；
  - web mode：现代 target，产物更小；
- 本步**保留 Plasmo 与 ZXing fallback** 不动。

### 步骤 5：零拷贝 ContentManifest（-300~600 行）

**拒绝 `RecoveredPayload<Vec<u8>>` 全量返回**——会破坏 Android/Windows 分段路径的磁盘流式有界内存设计。

```rust
pub enum ContentKind {
    Text { payload_offset: u64, payload_length: u64, utf8_valid: bool },
    Bundle { entries: Vec<BundleEntry> },
    SingleFile,
}

pub struct BundleEntry {
    pub name: String,
    pub payload_offset: u64,
    pub payload_length: u64,
}
```

- 双入口：
  - `inspect_payload_bytes(&[u8]) -> Result<ContentManifest>`（Web 及小文件内存块）
  - `inspect_payload_file(path) -> Result<ContentManifest>`（Android/Windows 磁盘大文件，仅读 header/metadata，零额外内存）
- Rust 负责：magic/version/count 校验、checked arithmetic、UTF-8 判定、越界守卫、尾部恰好消费、文本 magic 剥离范围；
- 客户端负责：按 offset/length 从内存 slice 或文件 range 读取、落盘、文件名路径安全、UI 分类；
- 注意：ETBUNDL1 无前置索引表（`[name_len][name][size][data]` 逐条目顺序排列），file 版 inspect 是 **O(N) 交错头遍历**（4096 条目 = 4096 次 seek），实现注释写明；
- 三端删除 `TextParser`/`BundleParser`，按 manifest 切片落盘与 UI 分流；测试移植为 manifest 版 + golden fixture。

### 步骤 6：三端控制器拆解（-800~1,500 行）

**先行固化怪癖为事件序列测试**（硬约束，逐字保留、禁止顺手"修"）：

- `multiMiss % 3 == 0` 的全帧解码调度（实测优于 ROI 优先，勿改判定条件）；
- 切流重锁的 1.5s 静默防抖（`RelockSilenceTicks`）；
- 17 帧描述符间隔（与 2/4 多码布局互质，勿改回 16）;
- mismatch relock 阈值 ≥3。

**分层结构：**

```
CapturePipeline    只拥有摄像头/屏幕源与 QR 解码线程池
       │ FrameDecoded(payloads)
ReceiveCoordinator 纯同步事件状态机（唯一可单测核心）：
                   SessionGuard 重锁防抖 / 帧级去重 / 秒判跳过 /
                   ContinuousSaver 调度
       │
TransferMetrics    滑动窗口瞬时速率与统计
RecoveryService    assemble / decompress / ContentManifest / archive
UI Adapter         ScanViewModel / ScanActivity / ReceivePage：
                   仅生命周期、屏幕常亮、XAML/Compose/React 状态绑定
```

- **Windows 先试点**（net8.0 测试基建最好）→ 验证分层边界成立 → 复制 Android → Web；
- 并发规则落为测试与所有权（不是重构后的默认假设）：
  - worker 回调只投递事件，不直接 stop/dispose；
  - `StopAsync` 只由 owner 调用；
  - UI 线程永不同步等待 worker；
  - session handle 只由一个串行执行器访问；
  - recovery 绑定 CancellationToken / AbortController；
  - 禁止 worker 内调用自己的 join/awaitTermination；
  - 尽量用 mailbox/channel 替代多把锁。

### 步骤 7：前端 package 合并（-80~150 行，暂留 Plasmo）

- 合并为 `apps/frontend/`：
  - 单 `package.json` + 单 `package-lock.json`；
  - 删 `pnpm-lock.yaml`（-6,159 行生成内容，不计入手写减量）；
  - 删 `../sender/src` 跨目录 alias 与重复的 `extract-lzma-wasm.cjs`；
  - **Plasmo 扩展构建行为不变**；
- 同步修改 CI 路径（pages.yml / windows.yml）。

### 步骤 8：Vite 替换 Plasmo（净减 100~250 行）

- 四 mode 构建：`web-sender` / `web-receiver` / `extension` / `standalone`；扩展产物用保守 target；
- manifest 用 base 模板 + 4 份 patch（chrome-mv2/mv3、firefox-mv2/mv3）；**version 字段构建时注入**（不进 version.mjs 同步清单，少一个事实源）；
- 验证清单：
  - options.html 路径与 background 打开；
  - MV2 `background.scripts` vs MV3 service_worker；
  - 各自 CSP（MV2 `wasm-eval`）；
  - Firefox `browser_specific_settings`；
  - CRX 固定 key 签名；
  - Firefox `web-ext lint`；
  - WASM / Worker URL；
  - Chrome 87 / 现代 Chrome / FF 91 / 现代 FF 实机加载：点图标 → 开 options → WASM 初始化 → 出 QR 帧；
- 接受损失 Plasmo HMR：dev = `vite build --watch` + 重载 unpacked 目录，写入 BUILD.md；
- 净减有限（删 ~300 行 Plasmo 脚本，新增 manifest 生成与打包代码），收益主要是构建体系统一。

### 步骤 9：FAST-only 强制（-120~220 行）

- **构建契约**：`airferry_zxing.js/.wasm` 缺失时 `build:receiver` 与 dev 均**立即失败**（不 warn 不降级）；
- `build-fastzxing.sh` 增产物完整性校验；CI 固定 emsdk 3.1.64；
- 彻底卸载 `zxing-wasm` 及全部兼容分支：
  - `qr-decode.worker.ts`：删 `ensureZxingWasm` / `cropRgba` / RGBA 路径，专注 `decodeFastY`；
  - `ReceivePage.tsx`：删 `fastBackendRef` 分支，无条件 `extractYPlane`；
  - `prepare-wasm.cjs` / `build-standalone.cjs`：删 `zxing_reader.wasm` 拷贝逻辑；
- 开发机一次性安装 emsdk 写入 BUILD.md；fastzxing 产物缓存后日常开发无需重复编译；
- 补 FAST 回归测试：单码/四码、1080p、旋转/透视、空白帧、截断图、错误 stride、连续坏帧 worker 不永久退出（池自愈）。

### 步骤 10：文档收敛 + 产物校验

**去重事实，而非凑文件数：**

| 文件 | 定位 |
|---|---|
| `docs/SPEC.md` | 唯一线协议事实源（并入 protocol / qr-frame-format / raptorq-params） |
| `docs/architecture.md` | 组件边界与数据流（并入 data-flow） |
| `docs/BUILD.md` | 公共构建入口（吸收 dev-setup 与通用前置） |
| `AGENTS.md` | 压至 ≤150 行，只留导航 + 硬规则；坑清单迁移至各权威文档；源码行号引用改符号名 |
| `docs/benchmarks/` | `perf-web-receiver.md` 移入（FAST-only 决策的性能证据，保留不删） |
| `docs/releases/*.md` | 历史发布记录，不参与合并 |

- `scripts/verify-dist.mjs` **新增门禁**（与 `dist-upload-list` **并存**——后者仍是 `gh release upload` 的输入）：
  - 所有期望产物存在；
  - zip/xpi/crx 内 manifest 正确；
  - FAST WASM 确实在接收端产物内；
  - APK 非 debug 签名；
  - CRX 使用固定 key；
  - 归档不含 pem/keystore；
  - 版本一致。

---

## 明确不做（关闭反复讨论）

- **`uniffi-rs`**：无 C# 后端，无法服务 Windows 端，不可用；
- **FFI 绑定代码生成**（proc-macro / build script）：快照化后每端仅 5~6 个导出，drift 压力已消除，不值得引入生成机器。

## 每步门禁（全绿才进下一步）

```bash
# Rust
cargo test
cargo test -p transfer-engine --features cffi
cargo test -p transfer-engine --features jni

# Web
npm run typecheck && npm test
npm run build && npm run build:receiver && npm run build:standalone

# Android
cd apps/scanner && ./gradlew :app:testDebugUnitTest :app:assembleDebug

# Windows
dotnet test apps/windows/AirFerry.Windows.Tests   # 任意 OS
# + CI windows runner 完整构建

# 全局
node scripts/version.mjs check
node scripts/verify-dist.mjs
# golden fixture 三端通过
```

**回滚策略**：每步小 PR 系列，git revert 即回滚；每步结束都是可发布状态。

## 减量估算

| 项目 | 预计净减手写行数 |
|---|---:|
| Rust 死代码 | 750~850 |
| FFI snapshot | 500~750 |
| WASM 单轨 | 150~250 |
| FAST-only | 120~220 |
| 前端 package 合并 | 80~150 |
| 移除 Plasmo 后净减 | 100~250 |
| ContentManifest 下沉 | 300~600 |
| 控制器三端拆解 | 800~1,500 |
| **合计** | **2,800~4,600** |

文档另减约 700~1,100 行。`pnpm-lock.yaml` 6,159 行可删但属生成内容，不计入手写减量。

## 风险分层提示

- **低风险高收益核心（~60% 收益）**：步骤 1~5 + 7；
- **高风险可维护性投资**：步骤 6（三端控制器）、8（Vite 替换）、9（FAST 强制）；
- 若中途有版本压力，可停在任意已完成步骤，仓库状态仍净改善。

## 结果衡量指标（优于单纯行数）

- 手写生产代码 LOC；
- FFI 导出函数数量；
- 协议解析实现份数；
- package/lockfile 数量；
- 构建目标数量与总构建时间；
- 大控制器圈复杂度；
- 全量构建所需工具数量；
- 端到端测试覆盖的平台矩阵。
