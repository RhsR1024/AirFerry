/**
 * AF2 File-preparation worker.
 *
 * Reads user-selected files or text off the main thread, normalizes paths and
 * emits entries `{ kind, path, content }` ready for `SenderBuilderWasm`.
 */

/// <reference lib="webworker" />

export const KIND_FILE = 1
export const KIND_UTF8_TEXT = 2
export const KIND_DIRECTORY = 3

export interface PreparedItem {
  kind: number
  path: string
  content: ArrayBuffer
}

export interface CompressResult {
  jobId: number
  items: PreparedItem[]
  totalBytes: number
  displayName: string
}

function post(msg: unknown, transfer: Transferable[] = []): void {
  ;(postMessage as (m: unknown, transfer?: Transferable[]) => void)(msg, transfer)
}

self.addEventListener("message", async (e: MessageEvent) => {
  const data = e.data
  if (!data || typeof data !== "object") return

  if (data.type === "wasm-init") {
    post({ phase: "ready" })
    return
  }

  const { jobId, files, text, name } = data as {
    jobId: number
    files?: File[]
    text?: string
    name?: string
  }

  try {
    post({ phase: "reading", jobId })

    const items: PreparedItem[] = []
    let totalBytes = 0
    let displayName = "传输内容"

    if (typeof text === "string") {
      // NFC-normalize: the AF2 manifest validates paths as Unicode NFC and
      // rejects combining marks (macOS delivers NFD filenames by default).
      const cleanName = (name || "文字消息.txt").trim().normalize("NFC")
      displayName = cleanName
      const encoded = new TextEncoder().encode(text)
      totalBytes = encoded.byteLength
      items.push({
        kind: KIND_UTF8_TEXT,
        path: cleanName,
        content: encoded.buffer,
      })
    } else if (Array.isArray(files) && files.length > 0) {
      displayName = files[0].name
      if (files.length > 1) {
        displayName = `${files[0].name} 等 ${files.length} 个文件`
      }
      const usedPaths = new Set<string>()
      for (const file of files) {
        if (file.size > 1024 * 1024 * 1024) {
          throw new Error(`单文件大小超过 1 GiB 上限: ${file.name} (${(file.size / (1024 * 1024)).toFixed(1)} MiB)`)
        }
        const buffer = await file.arrayBuffer()
        if (buffer.byteLength !== file.size) {
          throw new Error(`文件读取截断: ${file.name} 期望 ${file.size} 字节，实际读取 ${buffer.byteLength} 字节`)
        }
        let filePath = (file.name || "unnamed").normalize("NFC")
        if (usedPaths.has(filePath)) {
          let counter = 1
          const dotIdx = filePath.lastIndexOf(".")
          const stem = dotIdx > 0 ? filePath.substring(0, dotIdx) : filePath
          const ext = dotIdx > 0 ? filePath.substring(dotIdx) : ""
          while (usedPaths.has(`${stem} (${counter})${ext}`)) {
            counter++
          }
          filePath = `${stem} (${counter})${ext}`
        }
        usedPaths.add(filePath)
        totalBytes += buffer.byteLength
        items.push({
          kind: KIND_FILE,
          path: filePath,
          content: buffer,
        })
      }
    }

    const transfers = items.map((it) => it.content)
    post(
      {
        phase: "done",
        jobId,
        items,
        totalBytes,
        displayName,
      },
      transfers
    )
  } catch (err: unknown) {
    post({
      phase: "error",
      message: err instanceof Error ? err.message : String(err),
      jobId,
    })
  }
})
