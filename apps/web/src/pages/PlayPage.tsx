/** Page 3: live QR video stream playback (AF2 automatic playlist). */
import { useState } from "react"
import { QrStream, type QrStreamStats } from "@/components/QrStream"
import type { SenderSessionWasm } from "@/wasm/loader"
import type { TransferConfig } from "@/types"

interface Props {
  session: SenderSessionWasm
  config: TransferConfig
  totalBytes: number
  onStop: () => void
}

export function PlayPage({
  session,
  config,
  totalBytes,
  onStop,
}: Props) {
  const [stats, setStats] = useState<QrStreamStats | null>(null)
  const [error, setError] = useState<string | null>(null)

  // Total bytes including redundancy estimate.
  const totalWithRedundancy = totalBytes * (1 + config.redundancyPct / 100)
  const passPct =
    stats && totalWithRedundancy > 0
      ? (stats.bytes / totalWithRedundancy) * 100
      : 0
  const progressPct = Math.min(100, passPct)
  const supplementing = passPct >= 100

  return (
    <div className="play-page">
      <div className="play-header">
        <h2>正在播放二维码流</h2>
        <p className="play-sub">
          接收端（手机或电脑扫码端）扫描任意画面即可随时加入并自动连续恢复
        </p>
      </div>

      {error && <div className="alert error">{error}</div>}

      <div className="stream-container">
        <QrStream
          session={session}
          fps={config.fps}
          brightness={config.brightness}
          autoOptimize={config.autoOptimize}
          multiQr={config.multiQr}
          ditherJitter={config.ditherJitter}
          onStop={onStop}
          onStats={setStats}
          onError={(e) => setError(e.message)}
        />
      </div>

      <div className="play-progress-card">
        <div className="progress-bar-bg">
          <div
            className={`progress-bar-fill ${supplementing ? "supplementing" : ""}`}
            style={{ width: `${progressPct}%` }}
          />
        </div>
        <div className="progress-meta">
          <span>
            {supplementing ? "持续发送修复符号 (Epoch 循环)" : `首轮发送进度 ${progressPct.toFixed(0)}%`}
          </span>
          {stats && stats.throughputBps > 0 && (
            <span>{(stats.throughputBps / 1024).toFixed(0)} KB/s</span>
          )}
        </div>
      </div>
    </div>
  )
}
