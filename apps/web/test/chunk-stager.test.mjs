import assert from "node:assert/strict"
import test from "node:test"

import { createChunkStager } from "../src/lib/chunk-stager.ts"

class FakeWorker {
  constructor() {
    this.listeners = new Map()
    this.sent = []
  }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set())
    this.listeners.get(type).add(fn)
  }
  removeEventListener(type, fn) {
    this.listeners.get(type)?.delete(fn)
  }
  postMessage(msg) {
    this.sent.push(msg)
  }
  dispatch(data) {
    for (const fn of this.listeners.get("message") ?? []) fn({ data })
  }
  requests() {
    return this.sent.filter((m) => m.type === "stage").map((m) => m.index)
  }
}

/** Scriptable fake of SenderSessionWasm's staging surface. */
function fakeSession() {
  const state = { cur: -1, epoch: 1, staged: new Set() }
  const stageCalls = []
  return {
    state,
    stageCalls,
    current_chunk_index: () => state.cur,
    epoch: () => state.epoch,
    is_staged: (i) => state.staged.has(i),
    stage_chunk(i, codec, data, rawHash) {
      stageCalls.push({ i, codec, data, rawHash })
      state.staged.add(i)
    },
    /** Simulate the core consuming staged bytes (window META emission). */
    consume(i) {
      state.staged.delete(i)
    },
  }
}

const stagedMsg = (jobId, index) => ({
  type: "staged",
  jobId,
  index,
  codec: 0,
  data: new Uint8Array(4),
  rawHash: new Uint8Array(32),
})

test("single-chunk: armed reconciles against is_staged, epoch flips never re-request or stall", () => {
  const worker = new FakeWorker()
  const session = fakeSession()
  const stager = createChunkStager({
    worker,
    session,
    jobId: 7,
    chunkCount: 1,
    isLive: () => true,
  })

  // Bootstrap: seeds chunk 0 (chunk 1 is out of range).
  assert.equal(stager.tick(session), true)
  assert.deepEqual(worker.requests(), [0])
  worker.dispatch(stagedMsg(7, 0))
  assert.equal(session.is_staged(0), true)

  // Window live, staged still armed: repeated ticks must NOT re-request
  // (the old key-change invalidation burned one worker round-trip per tick).
  session.state.cur = 0
  stager.tick(session)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0])

  // Window's META consumed the staged bytes → exactly one re-arm.
  session.consume(0)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 0])
  worker.dispatch(stagedMsg(7, 0))

  // Epoch wrap (same chunk index): the armed entry is still valid — the
  // epoch flip alone must not trigger a request (this was the per-epoch
  // staging stall).
  session.state.epoch = 2
  stager.tick(session)
  assert.equal(worker.sent.length, 2)
  // …and consumption at the new window's META re-arms exactly once.
  session.consume(0)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 0, 0])
})

test("multi-chunk: keeps only the actual next window staged (bounded memory)", () => {
  const worker = new FakeWorker()
  const session = fakeSession()
  const stager = createChunkStager({
    worker,
    session,
    jobId: 9,
    chunkCount: 3,
    isLive: () => true,
  })

  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 1])
  worker.dispatch(stagedMsg(9, 0))
  worker.dispatch(stagedMsg(9, 1))

  // Window 0 live: prefetch chunk 1 — already armed, nothing new.
  session.state.cur = 0
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 1])
  // Chunk 0 consumed at its window META. Do NOT immediately re-stage chunk 0
  // for the next epoch — doing that for every visited chunk accumulates the
  // whole file in staged_chunks and defeats the streamed sender's O(1-chunk)
  // memory bound. Chunk 1 is the only actual next window and is already armed.
  session.consume(0)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 1])

  // Window 1: consumption drops its stale armed marker; only chunk 2 is
  // prefetched.
  session.state.cur = 1
  session.consume(1)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 1, 2])
  worker.dispatch(stagedMsg(9, 2))

  // Last chunk's window: wraparound makes chunk 0 the actual next window, so
  // it is prefetched exactly here (and not retained for the whole epoch).
  session.state.cur = 2
  session.consume(2)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0, 1, 2, 0])
})

test("handleNotStaged requests the missing chunk once (inflight dedup) and dispose detaches", () => {
  const worker = new FakeWorker()
  const session = fakeSession()
  const stager = createChunkStager({
    worker,
    session,
    jobId: 11,
    chunkCount: 2,
    isLive: () => true,
  })
  session.state.cur = 1

  assert.equal(stager.handleNotStaged(new Error("AF2_CHUNK_NOT_STAGED:0")), true)
  assert.equal(stager.handleNotStaged(new Error("AF2_CHUNK_NOT_STAGED:0")), true)
  assert.deepEqual(worker.requests(), [0], "duplicate markers dedup while inflight")
  assert.equal(stager.handleNotStaged(new Error("something else")), false)

  // Stale-job replies are ignored; live replies stage into the session.
  worker.dispatch(stagedMsg(99, 0))
  assert.equal(session.is_staged(0), false)
  worker.dispatch(stagedMsg(11, 0))
  assert.equal(session.is_staged(0), true)

  stager.dispose()
  session.consume(0)
  stager.tick(session)
  assert.deepEqual(worker.requests(), [0], "dispose stops all requests")
})
