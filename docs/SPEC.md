# AirFerry Protocol 2 (AF2) 跨端位级契约规格 (SPEC)

> **权威位级线格式与跨端不变量清单**。
> Rust 核心（`core/af2/`、`core/transfer-engine/`）、Web/扩展前端（`apps/web/`）、Android 接收端（`apps/scanner/`）、Windows 接收端（`apps/windows/`）共享。
> 
> - **Wire magic / version**：`AF` / `2`
> - **兼容性**：与 v1 协议（`ET / wire version 1`）完全不兼容，按魔数互斥，无误解析窗口。

---

## 0. 核心架构裁决

1. **自举闭合**：晚加入接收端不依赖“先恢复某对象”即可拿到精确 OTI。
2. **编码实例隔离**：同内容换 T / 换压缩器重发时，未完成 Decoder 结构性不混流（以 128 位 `object_id` 为路由键，包含 `encoded_hash` 并在解码前复算比对）。
3. **独立提交**：Raw Chunk 独立分块、压缩、校验、落盘，支持跨重启与跨编码实例复用。
4. **位级严谨**：所有长度、加法、乘法、偏移、`ceil_div` 在切片或分配前必须经 checked arithmetic 防御。
5. **哈希基准**：采用 **BLAKE3-256 单算法**（Rust `blake3` crate 唯一权威，FFI 跨端复用，空输入摘要 `af1349b9...`）。

---

## 1. 目标与非目标

### 1.1 目标
1. 文件、UTF-8 文字、目录、多文件包采用统一的 Entry 模型，废除旧式魔数嗅探；
2. 用户选择内容后自动分块、压缩、编码、循环播放，无需手动换段；
3. 每个 Raw Chunk 独立恢复、解压、校验、落盘、跨重启复用；
4. 同内容换 T / codec / 压缩参数重发：已完成 Chunk 复用，未完成 Decoder 结构性不混流；
5. 一切不可信输入在进入第三方解码器前 fail-closed 验尽；
6. 协议演进通过 Critical/Optional TLV 与注册表扩展，消除固定尾部追加。

### 1.2 非目标
- 不提供双向 ACK/NACK（完全单向光学信道）；
- 不做符号级 Decoder 持久化（仅做 Chunk 级持久化）；
- 不提供对 v1 构件的升级兼容。

---

## 2. 分层模型

```text
内容层   Manifest：Entry、规范路径、Entry Hash、Chunk Hash Table
分块层   Canonical Content Stream → 固定大小 Raw Chunk (默认 8 MiB)
压缩层   每个 Raw Chunk 独立选择 RAW / Zstd / XZ
FEC 层   Manifest 与每个 Encoded Chunk 各为一个 RaptorQ Object（RFC 6330）
帧层     ROOT / OBJECT_META / SYMBOL
物理层   每帧一个 QR；多码布局并行承载多帧
```

- **Entry**：文件、UTF-8 文字或目录。
- **Canonical Content Stream**：按规范路径字节序升序，顺序无缝拼接全部非目录 Entry 内容。
- **Raw Chunk**：该流的固定范围切片。**Encoded Chunk**：Raw Chunk 经 RAW/Zstd/XZ 编码后的字节。
- **Object**：独立 RaptorQ 对象；Manifest 是一个，每个 Encoded Chunk 各是一个。
- **Broadcast Instance**：以固定 T 与一组固定 Object Meta 连续播放一个 Transfer 的一次运行。
- **Epoch**：调度器遍历全部 Chunk 一轮；后续 Epoch 发送各对象未用过的新 Repair ESI。

---

## 3. 基础编码约定

- 多字节整数一律大端序（Big-Endian）；`u24` 为 3 字节大端无符号数。
- 基础哈希：**BLAKE3-256**（记作 `H(...)`，输出 32B，空输入摘要 `af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262`）。
- 帧校验：CRC-32/ISO-HDLC (IEEE)（自检 `CRC32("123456789") = 0xCBF43926`）。
- 字符串：严格 UTF-8；路径分隔符固定 `/`；域标签为精确 ASCII 字节。
- `Trunc128(h)`：取 32B 哈希前 16B。

---

## 4. 三层身份体系

| ID | 位宽 | 回答的问题 | 编码参数变化时 |
|---|---:|---|---|
| **Content ID** | 256 | 逻辑内容与路径结构指纹 | 保持不变 |
| **Transfer ID** | 128 | 该内容按指定 `chunk_raw_size` 切分的身份 | 仅块大小变化才变 |
| **Object ID** | 128 | 某 Object（Manifest 或 Encoded Chunk）的一次精确编码 | 任意参数变化即变 |

### 4.1 Content ID
```text
content_id = H(
    ASCII("AF2/content/v1")
    || entry_count:u32
    || repeated { kind:u8 || path_len:u16 || path || size:u64 || entry_hash:[32] }
)
```
*注：目录 `size = 0`、`entry_hash = H(空)`。mtime、MIME、权限不进身份，仅作为 TLV 注解。*

### 4.2 Transfer ID
```text
transfer_id = Trunc128(H(
    ASCII("AF2/transfer/v1") || manifest_hash:[32] || chunk_raw_size:u32
))
```

### 4.3 Object ID
```text
object_id = Trunc128(H(
    ASCII("AF2/object/v1")
    || transfer_id:[16] || role:u8 || object_index:u32
    || codec_id:u8 || fec_id:u8 || oti:[12] || encoded_hash:[32]
))
```

---

## 5. Wire Frame 格式

```text
[Header 26 B][Payload Area T B][Frame CRC32 4 B]     总开销 30 B
```

| 偏移 | 长度 | 字段 | 说明 |
|---:|---:|---|---|
| 0 | 2 | magic | ASCII `AF` (`0x4146`) |
| 2 | 1 | wire_version | 固定 `2` |
| 3 | 1 | frame_type | `1`=ROOT, `2`=OBJECT_META, `3`=SYMBOL |
| 4 | 16 | object_id | SYMBOL/META = Object ID；ROOT = Transfer ID |
| 20 | 2 | body_len | Payload Area 内有效字节数 |
| 22 | 1 | sbn | RaptorQ SBN（控制帧必须 0） |
| 23 | 3 | esi | RaptorQ ESI，u24 BE（控制帧必须 0） |
| 26 | T | payload_area | 有效 body + 零填充至 T 字节 |
| 26+T| 4 | frame_crc32 | 覆盖 0..26+T 字节的 IEEE CRC32 |

- `T`: `256 ≤ T ≤ 2400` 且 `T % 8 == 0`；一个 Broadcast Instance 内恒定。
- SYMBOL 帧 `body_len == T`；控制帧 `body_len ≤ T` 且 `body_len..T` 填充 0。

---

## 6. 控制记录格式

### 6.1 Root Record (`AFR2`, 112 B + TLV)
| 偏移 | 长度 | 字段 |
|---:|---:|---|
| 0 | 4 | magic ASCII `AFR2` |
| 4 | 1 | schema = 1 |
| 5 | 1 | flags = 0 |
| 6 | 2 | fixed_len = 112 |
| 8 | 2 | extensions_len |
| 10 | 2 | reserved = 0 |
| 12 | 32 | content_id |
| 44 | 16 | manifest_object_id |
| 60 | 32 | manifest_hash |
| 92 | 8 | total_raw_size |
| 100 | 4 | entry_count |
| 104 | 4 | chunk_count |
| 108 | 4 | chunk_raw_size (默认 8 MiB) |

### 6.2 Object Meta Record (`AFO2`, 112 B + TLV)
| 偏移 | 长度 | 字段 |
|---:|---:|---|
| 0 | 4 | magic ASCII `AFO2` |
| 4 | 1 | schema = 1 |
| 5 | 1 | role: `1`=MANIFEST, `2`=CHUNK |
| 6 | 2 | fixed_len = 112 |
| 8 | 2 | extensions_len |
| 10 | 2 | reserved = 0 |
| 12 | 16 | transfer_id |
| 28 | 4 | object_index |
| 32 | 1 | codec_id (0=RAW, 1=Zstd, 2=XZ) |
| 33 | 1 | fec_id (固定 1 = RaptorQ RFC 6330) |
| 34 | 2 | reserved = 0 |
| 36 | 12 | oti (RFC 6330 12B 线格式) |
| 48 | 32 | raw_hash |
| 80 | 32 | encoded_hash |

### 6.3 Manifest Header (`AFM2`, 80 B)
- 恒为 RAW 不压缩，上限 16 MiB。
- 结构：`[Header 80 B][Entry Records][Chunk Hash Table][Manifest TLVs]`。
- **Entry Record (60 B + path + TLV)**：`kind` (1=FILE, 2=UTF8_TEXT, 3=DIRECTORY)、`content_offset`、`content_size`、`content_hash`。
- **路径约束**：Unicode NFC、严格 UTF-8、`/` 分隔、禁止 `..` 与绝对路径、总长 ≤ 1024 B、单段 ≤ 255 B。

---

## 7. 压缩编解码与调度

### 7.1 压缩注册表
| codec_id | 算法 | 约束 |
|---:|---|---|
| 0 | RAW | encoded == raw |
| 1 | Zstd | 单 Frame、windowLog ≤ 23 |
| 2 | XZ/LZMA2 | 单 Stream、解码内存 ≤ 128 MiB |

- 严格变小才压缩：压缩结果严格小于原始大小时方可使用压缩，否则必须为 RAW。

### 7.2 标准 Playlist 调度
```text
Bootstrap: ROOT × 4 → MANIFEST META × 4 → up to 32 Manifest Symbols
Each Chunk i: ROOT × 1 → CHUNK i META × 2 → i's source symbols → fresh repair symbols (0.25 K)
Interleave: META 每 ~17 帧广播；ROOT 每 ~31 帧广播；每 ~8 个 Chunk Symbol 插入 1 个 Manifest Symbol
```

---

## 8. 完整性校验链（强制顺序）

```text
① Frame CRC32 
  → ② Header/Meta 边界与预算校验 
  → ③ OTI 校验（先于建 Decoder）
  → ④ object_id + encoded_hash（解码前验证 META，恢复后验证字节）
  → ⑤ 解压窗口/内存/精确长度/尾随校验 
  → ⑥ Chunk Hash（校验 Manifest 表）
  → ⑦ Manifest Hash（校验 ROOT） 
  → ⑧ Entry Hash 
  → ⑨ Content ID 重算
```

---

## 9. 跨端职责分工

- **Rust Core 唯一实现**：帧/ROOT/META/Manifest/TLV 编解码、哈希与三层 ID 派生、路径清洗、OTI 验证、状态机、有界解压接口、单一 Receiver Snapshot ABI。
- **宿主层（TS / Kotlin / C#）**：相机与屏幕捕获、QR 灰度解码、文件系统与 IndexedDB 落盘、UI 视图。禁止在宿主语言中镜像线格式协议。
