# API 参考 (API Reference)

## Rust 核心 API

### raptorq-core

```rust
// 配置
pub struct Config { pub symbol_size: u32 }  // 默认 1024；浏览器端按速度预设传入（默认 1400）
pub const DEFAULT_SYMBOL_SIZE: u32 = 1024;

// 编码器
pub struct Encoder { ... }
impl Encoder {
    pub fn new(data: &[u8], config: Config) -> Result<Self>;
    pub fn meta(&self) -> &ObjectMeta;
    pub fn source_symbol(&self, sbn: u32, esi: u32) -> Result<Symbol>;
    // 任意合法 start 偏移 → 按需生成新鲜修复符号（ESI < 2^24）
    pub fn repair_symbols(&self, sbn: u32, start: u32, count: u32) -> Result<Vec<Symbol>>;
}

// 解码器
pub struct Decoder { ... }
impl Decoder {
    pub fn new(meta: ObjectMeta) -> Self;
    pub fn add_symbol(&mut self, symbol: &Symbol) -> Result<bool>;  // 返回是否完成
    pub fn is_complete(&self) -> bool;
    pub fn assemble(&self) -> Option<Vec<u8>>;
}

// 元数据
pub struct ObjectMeta {
    pub transfer_length: u64,
    pub symbol_size: u32,
    pub oti_bytes: [u8; 12],
    pub blocks: Vec<SourceBlockMeta>,
}
```

### qr-protocol

```rust
// 帧
pub struct Frame { pub header: FrameHeader, pub payload: Vec<u8>, pub frame_crc32: u32 }
impl Frame {
    pub fn build(session_id, flags, sbn, esi, ...) -> Self;
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<Self>;  // 含 magic + CRC 校验
}

// QR 矩阵：按帧长选能容纳的最小 EC-L 版本（非固定 V40）
pub fn min_version_for(len: usize) -> Option<Version>;
pub fn encode(data: &[u8]) -> Result<QrMatrix>;

// 会话 ID
pub fn derive(name, size, mtime, fingerprint) -> SessionId;

// 压缩（native/Android；浏览器端在 TS 层实现）
// 算法标签：0=None, 1=Zstd, 2=XZ
pub const COMPRESSION_NONE: u8 = 0;
pub const COMPRESSION_ZSTD: u8 = 1;
pub const COMPRESSION_XZ: u8 = 2;

pub fn compress_with(data: &[u8], compression: u8) -> Result<Vec<u8>>;
pub fn decompress_with(data: &[u8], compression: u8) -> Result<Vec<u8>>;
```

### af2 & transfer-engine (Rust)

```rust
// AF2 发送端（core/af2）
pub struct Af2Sender { ... }
impl Af2Sender {
    pub fn new(
        items: Vec<(u8, String, Vec<u8>)>, // (kind, path, content); kind: 1=File, 2=UTF8Text, 3=Dir
        config: SenderConfig,               // symbol_size (256..=2400, %8==0), chunk_raw_size, redundancy_pct
    ) -> Result<Self, SenderError>;
    // 自动 playlist：ROOT×4 → META×4 → Manifest symbols → 逐 Chunk (ROOT×1 + META×2 + symbols)
    // 跨 epoch 循环，持续产生新鲜修复符号（ESI 单调递增永不重复，触 2^24 停止）
    pub fn next_frame(&mut self) -> Result<Vec<u8>, SenderError>;
    pub fn transfer_id(&self) -> [u8; 16];
    pub fn content_id(&self) -> [u8; 32];
}

// 传输引擎接收会话（core/transfer-engine）
pub struct ReceiverSession { ... }
impl ReceiverSession {
    pub fn new() -> Self;
    // 摄入单帧字节，返回 packed u64 状态字（bit 0: complete, 1: accepted, 2: manifest_ready, 3: chunk_ready, 8..23: mismatch_streak, 32..63: received_symbols）
    pub fn ingest(&mut self, frame_bytes: &[u8]) -> u64;
    pub fn is_complete(&self) -> bool;
    pub fn snapshot_json(&self) -> String; // Schema 2 快照
    pub fn progress(&self) -> Progress;
    pub fn assemble_chunk(&mut self, index: u32) -> Option<Vec<u8>>;
    pub fn forget_chunk(&mut self, index: u32) -> bool; // 驱逐已落盘 chunk，内存常数化
    pub fn verify_chunk(&self, index: u32, raw: &[u8]) -> bool; // §11/§12 对 Manifest 哈希表验块
    pub fn verify_final_stream(&self, stream: &[u8]) -> bool;   // §13 ⑧⑨ 终验（条目哈希、UTF-8、Content ID）
    pub fn resume(&mut self, root_frame_bytes: &[u8], completed: &[u32]) -> bool; // §12 续传
    pub fn invalidate_chunk(&mut self, index: u32) -> bool;     // §12 重核失败作废已恢复块
}
```

## WASM 绑定（浏览器）

> 以下签名以 `apps/web/wasm-pkg/transfer_engine.d.ts` 为准（AF2 快照化接口）。发送端经 `SenderBuilderWasm` 收拢条目后构建会话，接收端经 `ReceiverSessionWasm`+`snapshot_json` 恢复。旧 v1 接口（`SenderSessionWasm` 构造参数压缩负载、`receiverFileName` 等逐字段 getter）已移除。

```typescript
// SenderBuilderWasm（发送端：收拢条目 → 构建会话）
class SenderBuilderWasm {
  constructor()
  add_entry(kind: number, path: string, content: Uint8Array): void
  // kind: 1=文件, 2=UTF-8 文字, 3=目录
  build(symbol_size: number, chunk_raw_size: number, redundancy_pct: number): SenderSessionWasm
}

// SenderSessionWasm（发送端帧流）
class SenderSessionWasm {
  next_qr_scratch(count: number): number // 预分配 scratch；热路径跨帧复用
  qr_scratch_view(): Uint8Array          // 借用当帧矩阵（下一次 next_qr_scratch 即失效，勿缓存）
  stats_json(): string                   // {bytes, frames, elapsed_ms, fps, throughput_bps}
  content_id_hex(): string
  transfer_id_hex(): string
}

// Sha256Wasm（独立哈希）
class Sha256Wasm {
  constructor()
  update(bytes: Uint8Array): void
  digest(): Uint8Array
}

// QR 编码（独立函数）
function encode_qr(frame_bytes: Uint8Array, out_side: Uint32Array): Uint8Array
// 返回扁平模块网格（1=深色, 0=浅色），out_side[0] = 边长

// ReceiverSessionWasm（接收端，网页接收端用，AF2 状态机）
class ReceiverSessionWasm {
  constructor()

  // 摄入一帧解码后的 QR 原始字节（26B header + payload + 4B CRC）。
  // 返回 packed bigint（64位状态字）：bit 0: complete, 1: accepted, 2: manifestReady,
  // 3: chunkReady, 8..23: mismatchStreak, 32..63: receivedSymbols
  ingest(frame_bytes: Uint8Array): bigint

  // 单 JSON 快照（ReceiverSnapshotV2）：schema_version / meta_confirmed /
  // transfer_id_hex / content_id_hex / root_frame_hex / total_raw_size / entry_count /
  // chunk_count / chunk_raw_size / symbol_size / legacy_peer_frames / entries[{kind,path,save_path,offset,size}]
  snapshot_json(): string

  is_complete(): boolean
  last_chunk_index(): number
  assemble_chunk(index: number): Uint8Array
  forget_chunk(index: number): boolean
  verify_chunk(index: number, raw: Uint8Array): boolean
  verify_final_stream(stream: Uint8Array): boolean
  resume(root_frame_bytes: Uint8Array, completed_indices: Uint32Array): boolean
  invalidate_chunk(index: number): boolean
}
```

> **WASM 接收端内置纯 Rust 解压**：AF2 下支持全部三种 codec（RAW、Zstd 经 ruzstd、XZ 经 lzma-rs），
> Web 接收端已具备解压全部广播的能力；发送端暂以 RAW 发送（单向信道上 RAW 恒合法）。

> **构造参数来源**：条目由 `src/workers/compress.worker.ts`（AF2 file-preparation
> worker）离线读取产出 `PreparedItem[]`（`{ kind, path, content }`），主线程点「发送」
> 后 `postMessage`（带 `jobId` = 当前 epoch）：
> - `{ jobId, text, name? }` → 单条文字：`kind=KIND_UTF8_TEXT(2)`，path 用选择页命名（缺省 `文字消息.txt`）
> - `{ jobId, files }` → 文件列表：逐文件读取，重名自动加 ` (N)` 后缀；`kind=KIND_FILE(1)`
> - `{ type: "wasm-init", zstd?: ArrayBuffer | null }`：主线程始终发送，worker 标记 ready
> - 过期 `jobId` 的 progress/`done`/`error` 在 worker 与主线程双侧丢弃（改列表/回选择页 bump epoch）
> 然后 `options.tsx` 用 `SenderBuilderWasm.add_entry` + `build(symbol_size, chunk_raw_size, redundancy_pct)`
> 构建会话。详见 [SPEC.md](SPEC.md) 与 [architecture.md](architecture.md#数据流)。

## JNI 绑定（Android）

```kotlin
object NativeBridge {
    const val NATIVE_ABI_VERSION = 2   // 2: 快照化 FFI（receiverSnapshotJson 取代逐字段 getter）

    /** 启动期握手：断言 nativeAbiVersion() >= NATIVE_ABI_VERSION，否则进入 ErrorScreen。 */
    external fun nativeAbiVersion(): Int

    /** 创建接收会话；返回不透明指针（Long）。 */
    external fun receiverCreate(sessionIdLo: Long, sessionIdHi: Long): Long

    // 摄入一帧；返回 packed Long：完成/接受/manifestReady/chunkReady/mismatch/已收符号数
    external fun receiverIngest(handle: Long, frameBytes: ByteArray): Long

    // UI 约 7Hz 拉取完整进度；NUL 结尾 JSON，native 失败可返回 null/空数组
    external fun receiverProgressJson(handle: Long): ByteArray?

    external fun receiverIsComplete(handle: Long): Int

    // 单 JSON 接收快照（ReceiverSnapshotV2）：文件名/大小/root_frame_hex/
    // 块大小/符号大小/条目表（含 save_path）/legacy_peer_frames 一次取全
    external fun receiverSnapshotJson(handle: Long): String?

    external fun receiverAssembleBytes(handle: Long): ByteArray?
    external fun receiverAssembleChunk(handle: Long, index: Int): ByteArray?
    external fun receiverLastChunkIndex(handle: Long): Int
    external fun receiverForgetChunk(handle: Long, index: Int): Boolean
    external fun receiverVerifyChunk(handle: Long, index: Int, rawBytes: ByteArray): Boolean
    external fun receiverVerifyFinalStream(handle: Long, streamBytes: ByteArray): Boolean
    external fun receiverResume(handle: Long, rootFrameBytes: ByteArray, completedIndices: IntArray): Boolean
    external fun receiverInvalidateChunk(handle: Long, index: Int): Boolean

    external fun receiverDestroy(handle: Long)
}

object ZxingDecoder {
    external fun decodeY(
        yPlane: ByteArray, width: Int, height: Int, rowStride: Int
    ): ByteArray?  // 解码载荷或 null；native 按 rowStride 读取完整 Y 平面
}
```

> **线程模型**：`receiverIngest`/`receiverAssembleBytes` 等操作同一原生句柄，**非线程安全**。Android 侧用一把 ingest 锁串行化所有调用，ZXing 解码则在多个 worker 上并行（见 [architecture.md](architecture.md#数据流)）。

> **快照消费模式**：UI 以约 7Hz 拉取 `receiverSnapshotJson`（原子、单 JSON）；恢复期按
> `entries` + `receiverAssembleChunk` 逐 chunk 落盘，不再解析字节级容器。

### 进度 JSON 格式

`receiverProgressJson` 返回的 JSON（`receiverIngest` 只返回 packed `Long`）：

```json
{
  "decoded_symbols": 50,
  "total_symbols": 100,
  "received_symbols": 60,
  "frames_seen": 75,
  "frames_duplicate": 10,
  "frames_corrupt": 5,
  "decoded_blocks": 2,
  "total_blocks": 4,
  "decoded_fraction": 0.5,
  "loss_ratio": 0.2,
  "complete": false,
  "meta_confirmed": true,
  "session_mismatch_streak": 0
}
```
