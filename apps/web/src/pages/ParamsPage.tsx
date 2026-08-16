/** Page 2: transfer parameters (redundancy, fps, symbol size, brightness). */
import { TextDocIcon } from "@/components/icons"
import type { PendingItem, TransferConfig } from "@/types"
import { SPEED_PRESETS, presetForSymbolSize } from "@/types"

interface Props {
  /** Pending items that were staged (files + text). */
  items: PendingItem[]
  /** Display name for the transfer (filename, text name, or "N个文件打包"). */
  displayName: string
  /** Total original byte count of the transfer unit. */
  originalSize: number
  /** Whether the payload is a multi-file bundle. */
  isBundle: boolean
  /** Pure ETTEXTv1 text transfer (receiver copy/share UI). */
  isText: boolean
  /** Total segment count for a segmented large transfer (1 when non-segmented). */
  segmentCount?: number
  config: TransferConfig
  onChange: (patch: Partial<TransferConfig>) => void
  onStart: () => void
  /** Whether the WASM encoder is currently being initialized. */
  initializing?: boolean
}

function itemLabel(it: PendingItem): string {
  return it.kind === "file" ? it.file.name : it.name
}

function itemSize(it: PendingItem): number {
  return it.kind === "file"
    ? it.file.size
    : new TextEncoder().encode(it.content).length
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

function formatDuration(seconds: number): string {
  if (!isFinite(seconds) || seconds <= 0) return "—"
  const s = Math.ceil(seconds)
  if (s < 60) return `${s} 秒`
  const m = Math.floor(s / 60)
  const rem = s % 60
  if (m < 60) return `${m} 分 ${rem} 秒`
  return `${Math.floor(m / 60)} 小时 ${m % 60} 分`
}

export function ParamsPage({
  items,
  displayName,
  originalSize,
  isBundle,
  isText,
  segmentCount = 1,
  config,
  onChange,
  onStart,
  initializing
}: Props) {
  const isSegmented = segmentCount > 1

  // Pre-transfer ETA estimate (before encoder init). Chunk compression is
  // transparent in AF2 (per-chunk, decided by Rust), so estimate from the
  // original size — there is no meaningful "compressed size" to show here.
  const perSegmentBytes = isSegmented ? originalSize / segmentCount : originalSize
  const totalSymbols = Math.ceil(perSegmentBytes / config.symbolSize)
  const totalFrames = Math.ceil(totalSymbols * (1 + config.redundancyPct / 100))
  const effectiveFps = config.fps > 0 ? config.fps : 60 // conservative display-refresh estimate
  const estimatedSeconds = totalFrames / effectiveFps

  // Show the item list (collapsible-ish: first few + "还有 N 个" for bundles).
  const visibleItems = items.slice(0, 5)
  const hiddenCount = items.length - visibleItems.length

  const contentLabel = isText ? "文字内容" : isBundle ? "打包内容" : "文件"

  return (
    <div className="page">
      <h2>传输参数</h2>
      <table className="kv">
        <tbody>
          <tr>
            <td>{contentLabel}</td>
            <td>
              {displayName}
              {isBundle && (
                <ul className="kv-file-list">
                  {visibleItems.map((it) => (
                    <li key={it.id}>
                      <span>
                        {it.kind === "text" ? <TextDocIcon size={13} /> : null}
                        {itemLabel(it)}
                      </span>
                      <span className="muted"> {formatBytes(itemSize(it))}</span>
                    </li>
                  ))}
                  {hiddenCount > 0 && (
                    <li className="muted">…还有 {hiddenCount} 项</li>
                  )}
                </ul>
              )}
            </td>
          </tr>
          <tr>
            <td>原始大小</td>
            <td>{formatBytes(originalSize)}</td>
          </tr>
          {isSegmented && (
            <tr>
              <td>分段</td>
              <td>{segmentCount} 段 × ~32 MiB（压缩流切段，接收端自动合并）</td>
            </tr>
          )}
          <tr>
            <td>{isSegmented ? "每段预计帧数" : "预计帧数"}</td>
            <td>{totalFrames.toLocaleString()}</td>
          </tr>
          <tr>
            <td>{isSegmented ? "每段预计时间" : "预计传输时间"}</td>
            <td>
              <strong>{formatDuration(estimatedSeconds)}</strong>
              <span className="muted"> ({config.fps > 0 ? config.fps + "fps" : "跟随屏幕刷新"}, {config.redundancyPct}% 冗余)</span>
            </td>
          </tr>
        </tbody>
      </table>

      <div className="field">
        <div className="field-label">
          <span>冗余率 <span className="muted">(5%–50%)</span></span>
          <span className="field-value">{config.redundancyPct}%</span>
        </div>
        <input
          type="range"
          min={5}
          max={50}
          step={5}
          value={config.redundancyPct}
          onChange={(e) => onChange({ redundancyPct: Number(e.target.value) })}
        />
        {/* Loss-aware tuning hint. RaptorQ needs K unique symbols/block; at a
            given loss rate L the receiver keeps ~(1-L) of each pass, so the
            redundancy should at least cover the loss to finish in one pass. */}
      </div>

      <div className="field">
        <label>速度档位（每帧数据量）</label>
        <select
          value={presetForSymbolSize(config.symbolSize)?.id ?? "custom"}
          onChange={(e) => {
            const preset = SPEED_PRESETS.find((p) => p.id === e.target.value)
            if (preset) {
              // Apply both the symbol size and the preset's recommended fps.
              // The user can still nudge fps independently afterwards.
              onChange({ symbolSize: preset.symbolSize, fps: preset.fps })
            }
          }}
        >
          {SPEED_PRESETS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
          {/* Shown when symbolSize is a non-preset value (e.g. legacy 1024). */}
          {!presetForSymbolSize(config.symbolSize) && (
            <option value="custom">自定义（{config.symbolSize}B）</option>
          )}
        </select>
      </div>

      <div className="field">
        <label>帧率</label>
        <select
          value={config.fps}
          onChange={(e) => onChange({ fps: Number(e.target.value) })}
        >
          <option value={15}>15 FPS（低端设备）</option>
          <option value={20}>20 FPS（大码稳定）</option>
          <option value={30}>30 FPS</option>
          <option value={45}>45 FPS（推荐）</option>
          <option value={60}>60 FPS（高速）</option>
          <option value={90}>90 FPS（高刷屏）</option>
          <option value={120}>120 FPS（高刷屏）</option>
          <option value={0}>跟随屏幕刷新（不跳过可见帧）</option>
        </select>
      </div>

      <div className="field">
        <div className="field-label">
          <span>亮度</span>
          <span className="field-value">{config.brightness.toFixed(2)}x</span>
        </div>
        <input
          type="range"
          min={1}
          max={1.5}
          step={0.05}
          value={config.brightness}
          onChange={(e) => onChange({ brightness: Number(e.target.value) })}
        />
      </div>

      <div className="field">
        <label className="field-check">
          <input
            type="checkbox"
            checked={config.autoOptimize}
            onChange={(e) => onChange({ autoOptimize: e.target.checked })}
          />
          自动优化亮度 / 对比度 / 边距
        </label>
      </div>

      <div className="field">
        <label>同屏二维码数（多码加速）</label>
        <select
          value={config.multiQr > 1 ? 4 : 1}
          onChange={(e) => onChange({ multiQr: Number(e.target.value) > 1 ? 4 : 1 })}
        >
          <option value={1}>关闭（每帧 1 个，最稳）</option>
          <option value={4}>开启（每帧 4 个，~4× 吞吐）</option>
        </select>
      </div>

      <div className="field">
        <label className="field-check">
          <input
            type="checkbox"
            checked={config.ditherJitter}
            onChange={(e) => onChange({ ditherJitter: e.target.checked })}
          />
          亚像素抖动（防摩尔纹）
        </label>
      </div>

      <button
        className="btn primary page-cta"
        onClick={onStart}
        disabled={initializing}
      >
        {initializing ? "正在初始化编码器…" : "开始传输"}
      </button>
    </div>
  )
}
