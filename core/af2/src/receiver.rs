//! AF2 receiver state machine (protocol 2, §11 + §13).
//!
//! ```text
//! Idle ──valid ROOT──► Locked
//! Locked ├─ MANIFEST META ─► DecodeManifest ─recovered+all-verified─► ManifestReady
//!        └─ CHUNK META ────► DecodeChunk (may precede the Manifest)
//! ```
//!
//! Resource policy: at most one Manifest decoder and one active chunk decoder
//! (≤ 2 total); SYMBOLs for unknown object ids are dropped with ZERO caching;
//! completed objects ignore repeated META/SYMBOL cheaply; session mismatch
//! debounce follows v1 (3 consistent foreign-Transfer ROOTs to re-lock; data
//! frames never trigger a re-lock; T is bound at lock time only, so a foreign
//! ROOT with a different T can still re-lock). A same-transfer ROOT with a new
//! `manifest_object_id` switches the Broadcast Instance: ledger kept,
//! unfinished decoders dropped, T re-bound (§6).
//!
//! Integrity chain enforced here: ① frame CRC (frame.rs) → ② record boundary
//! checks (root/meta/manifest) → ③ OTI gate BEFORE building any decoder →
//! ④ object_id + encoded_hash binding (META-time and byte-time) → ⑤ bounded
//! decompression with exact length → ⑥ chunk hash against the Manifest table
//! → ⑦ manifest hash + content id against ROOT → ⑧⑨ entry hashes + Content ID
//! recomputation in [`verify_final_stream`] before hosts publish.

use crate::chunk::decode_chunk;
use crate::frame::{Af2Frame, FrameType};
use crate::id::hash;
use crate::manifest::Manifest;
use crate::meta::{ObjectMetaRecord, CODEC_RAW};
use crate::id::{content_id, EntryIdInput, KIND_DIRECTORY, KIND_UTF8_TEXT, ROLE_CHUNK, ROLE_MANIFEST};
use crate::root::RootRecord;
use raptorq::ObjectTransmissionInformation;
use raptorq_core::{Decoder, ObjectMeta, SourceBlockMeta, Symbol};

/// The integrity chain's verdict for one ingested frame.
#[derive(Debug, Clone, PartialEq)]
pub enum IngestEvent {
    /// Malformed frame / unknown object id / stale symbol — dropped.
    Dropped,
    /// First valid ROOT accepted; the transfer is locked.
    RootLocked,
    /// A ROOT for a different transfer arrived (debounce counter included).
    RootMismatch { streak: u32 },
    /// ≥3 consistent foreign ROOTs → the receiver re-locked to a new transfer.
    Relocked,
    /// A META passed the object_id binding; a decoder was built.
    MetaBound { role: u8, object_index: u32 },
    /// A ROOT for the SAME transfer with identical semantic fields but a new
    /// `manifest_object_id` (re-broadcast with a new T / new encoding): the
    /// ledger (completed chunks) is kept, unfinished decoders were dropped,
    /// and the T was re-bound to the new Broadcast Instance.
    InstanceSwitched,
    /// META failed the object_id binding (spoofed / mixed instance).
    MetaRejected,
    /// A symbol entered a live decoder.
    SymbolAccepted,
    /// The manifest object decoded and passed every verification.
    ManifestReady,
    /// A chunk decoded, verified (encoded_hash + chunk chain) and its RAW
    /// bytes are ready for the host ledger.
    ChunkReady { index: u32, raw: Vec<u8> },
    /// A chunk decoded but failed verification — dropped, not committed.
    ChunkRejected,
}

#[derive(Debug, thiserror::Error)]
pub enum Af2ReceiverError {
    #[error("receiver: OTI gate: {0}")]
    OtiGate(String),
    #[error("receiver: decoder: {0}")]
    Decoder(String),
    #[error("receiver: manifest hash mismatch (ROOT vs recovered bytes)")]
    ManifestHashMismatch,
    #[error("receiver: chunk encoded_hash mismatch")]
    ChunkEncodedHashMismatch,
    #[error("receiver: resume failed: {0}")]
    Resume(String),
}

/// Finalization failures for [`Af2Receiver::verify_final_stream`] (§13 ⑧⑨).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FinalizeError {
    #[error("finalize: ROOT/Manifest not ready")]
    NotReady,
    #[error("finalize: stream length {got} != total_raw_size {want}")]
    Length { want: u64, got: u64 },
    #[error("finalize: entry {index} content hash mismatch")]
    EntryHash { index: usize },
    #[error("finalize: entry {index} (UTF8_TEXT) is not valid UTF-8")]
    NotUtf8 { index: usize },
    #[error("finalize: recomputed content id != ROOT content id")]
    ContentId,
}

/// Session-mismatch debounce: 3 consistent foreign ROOTs re-lock (v1 lesson).
const MISMATCH_RELOCK_THRESHOLD: u32 = 3;

/// Build `ObjectMeta` from the 12B OTI alone (protocol C3: no block table on
/// the wire — RFC 6330 §4.4.1.2 partitioning derived deterministically).
///
/// Gate BEFORE constructing any decoder (§13 ③): transfer length ceilings,
/// symbol-size sanity, block-count consistency.
pub fn object_meta_from_oti(
    oti: &[u8; 12],
    max_transfer_len: u64,
) -> Result<ObjectMeta, Af2ReceiverError> {
    let info = ObjectTransmissionInformation::deserialize(oti);
    let f = info.transfer_length();
    let t = u64::from(info.symbol_size());
    let z = u32::from(info.source_blocks());
    if t == 0 || t > 65_528 || t % 8 != 0 {
        return Err(Af2ReceiverError::OtiGate(format!("bad symbol size {t}")));
    }
    if f == 0 || f > max_transfer_len {
        return Err(Af2ReceiverError::OtiGate(format!(
            "transfer length {f} out of 1..={max_transfer_len}"
        )));
    }
    if z == 0 || z > 255 {
        return Err(Af2ReceiverError::OtiGate(format!("source blocks {z}")));
    }
    // RFC 6330 §4.4.1.2: Kt = ceil(F/T); (KL, KS, ZL, ZS) = partition(Kt, Z).
    let kt = u32::try_from(f.div_ceil(t)).map_err(|_| {
        Af2ReceiverError::OtiGate(format!("Kt overflow for transfer length {f}"))
    })?;
    let (kl, ks, zl, zs) = raptorq::partition(kt, z);
    let _ = zs;
    let mut blocks = Vec::with_capacity(z as usize);
    for i in 0..z {
        let k = if i < zl { kl } else { ks };
        blocks.push(SourceBlockMeta {
            sbn: i,
            num_source_symbols: k,
            block_length: u64::from(k) * t,
        });
    }
    let meta = ObjectMeta {
        transfer_length: f,
        symbol_size: t as u32,
        oti_bytes: *oti,
        blocks,
    };
    // The full hostile-input gate (v1 meta.rs validate) runs before any
    // decoder touches these numbers — panic=abort lifeline.
    meta.validate()
        .map_err(|e| Af2ReceiverError::OtiGate(e.to_string()))?;
    Ok(meta)
}

/// The AF2 receiver state machine. Owns at most one Manifest decoder and one
/// active chunk decoder; all other symbols are dropped with zero caching.
pub struct Af2Receiver {
    root: Option<RootRecord>,
    mismatch_streak: u32,
    /// Transfer id of the foreign ROOT that owns the current streak; a
    /// different foreign transfer resets the debounce (alternating streams
    /// must never evict the lock — "3 *consistent* foreign ROOTs").
    mismatch_transfer: Option<[u8; 16]>,
    manifest_decoder: Option<Decoder>,
    manifest_meta: Option<ObjectMetaRecord>,
    manifest: Option<Manifest>,
    manifest_done: bool,
    chunk_decoder: Option<(u32, Decoder, ObjectMetaRecord)>,
    chunk_done: std::collections::HashSet<u32>,
    t: usize,
}

impl Default for Af2Receiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Af2Receiver {
    pub fn new() -> Self {
        Af2Receiver {
            root: None,
            mismatch_streak: 0,
            mismatch_transfer: None,
            manifest_decoder: None,
            manifest_meta: None,
            manifest: None,
            manifest_done: false,
            chunk_decoder: None,
            chunk_done: std::collections::HashSet::new(),
            t: 0,
        }
    }

    pub fn root(&self) -> Option<&RootRecord> {
        self.root.as_ref()
    }

    pub fn manifest(&self) -> Option<&Manifest> {
        self.manifest.as_ref()
    }

    /// Verify staged chunk bytes against the ROOT-bound Manifest chunk-hash
    /// table. Returns false when the Manifest is not known yet (the chunk
    /// stays staged but unverified) or when the hash differs. Hosts call this
    /// when the Manifest arrives after chunks have already been staged.
    pub fn verify_chunk(&self, index: u32, raw: &[u8]) -> bool {
        match &self.manifest {
            Some(m) => m.chunk_hashes.get(index as usize) == Some(&hash(raw)),
            None => false,
        }
    }

    /// §11: drop a previously-completed chunk from the ledger so a later
    /// epoch can re-supply it. Used when post-manifest re-verification fails
    /// (a chunk whose META raw_hash was self-consistent but contradicts the
    /// Manifest table). Returns whether the index was in the ledger.
    pub fn invalidate_chunk(&mut self, index: u32) -> bool {
        self.chunk_done.remove(&index)
    }

    /// The wire symbol size T observed from the first accepted frame
    /// (0 before any frame is accepted).
    pub fn symbol_size(&self) -> usize {
        self.t
    }

    /// Ingest one raw QR payload. Never panics on hostile input.
    ///
    /// T binding rule (§5): the payload-area size T is only frozen when a
    /// transfer is LOCKED (first legal ROOT / resume). Data frames (META /
    /// SYMBOL) for an unlocked receiver are dropped without caching, and a
    /// foreign ROOT arriving with a different T can still trigger the 3-ROOT
    /// re-lock — a stray T from another broadcast must never wedge the
    /// session permanently.
    pub fn ingest(&mut self, frame_bytes: &[u8]) -> Result<IngestEvent, Af2ReceiverError> {
        let frame = match Af2Frame::from_bytes(frame_bytes) {
            Ok(f) => f,
            Err(_) => return Ok(IngestEvent::Dropped),
        };
        match frame.frame_type {
            FrameType::Root => self.on_root(frame),
            FrameType::ObjectMeta | FrameType::Symbol => {
                if self.root.is_none() {
                    // No legal ROOT yet: build NO decoder, cache NO symbol (§6).
                    return Ok(IngestEvent::Dropped);
                }
                debug_assert!(self.t != 0, "t is bound at lock time");
                if frame.t != self.t {
                    // T must be constant within a Broadcast Instance.
                    return Ok(IngestEvent::Dropped);
                }
                match frame.frame_type {
                    FrameType::ObjectMeta => self.on_meta(frame),
                    FrameType::Symbol => self.on_symbol(frame),
                    FrameType::Root => unreachable!(),
                }
            }
        }
    }

    /// Rebuild a locked receiver from a persisted ROOT frame plus the ledger's
    /// completed-chunk bitmap (§12 resume). The ROOT frame re-runs the full
    /// parse + id-binding path, so a tampered ledger cannot inject a fake
    /// transfer. Unfinished decoders are NOT restored (chunk-level resume
    /// only, per §1.2 non-goals); the sender's next epoch re-supplies symbols.
    ///
    /// Returns the number of completed indices actually applied (out-of-range
    /// indices are ignored) so the caller's ledger cannot over-count.
    pub fn resume(
        &mut self,
        root_frame_bytes: &[u8],
        completed: &[u32],
    ) -> Result<usize, Af2ReceiverError> {
        if self.root.is_some() {
            return Err(Af2ReceiverError::Resume(
                "receiver already locked; resume before ingesting".into(),
            ));
        }
        let frame = Af2Frame::from_bytes(root_frame_bytes)
            .map_err(|e| Af2ReceiverError::Resume(e.to_string()))?;
        if frame.frame_type != FrameType::Root {
            return Err(Af2ReceiverError::Resume("stored frame is not a ROOT".into()));
        }
        let ev = self.on_root(frame)?;
        if !matches!(ev, IngestEvent::RootLocked) {
            return Err(Af2ReceiverError::Resume(format!(
                "stored ROOT did not lock cleanly: {ev:?}"
            )));
        }
        let chunk_count = self.root.as_ref().map(|r| r.chunk_count).unwrap_or(0);
        let mut applied = 0usize;
        for &index in completed {
            if index < chunk_count && self.chunk_done.insert(index) {
                applied += 1;
            }
        }
        Ok(applied)
    }

    fn on_root(&mut self, frame: Af2Frame) -> Result<IngestEvent, Af2ReceiverError> {
        let record = match RootRecord::parse(&frame.body) {
            Ok(r) => r,
            Err(_) => return Ok(IngestEvent::Dropped),
        };
        let transfer = record.transfer();
        // The ROOT header must carry its own transfer id — a spoofed id is
        // dropped on EVERY path (lock, duplicate, foreign), not just lock.
        if frame.object_id != transfer {
            return Ok(IngestEvent::Dropped);
        }
        match &self.root {
            None => {
                // No legal ROOT yet: build NO decoder, cache NO symbol (§6).
                self.root = Some(record);
                self.t = frame.t;
                self.mismatch_streak = 0;
                self.mismatch_transfer = None;
                Ok(IngestEvent::RootLocked)
            }
            Some(current) => {
                if current.transfer() == transfer {
                    // Same transfer: semantic fields must be identical; a
                    // changed manifest_object_id is a legal re-broadcast (§6).
                    let consistent = current.content_id == record.content_id
                        && current.manifest_hash == record.manifest_hash
                        && current.total_raw_size == record.total_raw_size
                        && current.entry_count == record.entry_count
                        && current.chunk_count == record.chunk_count
                        && current.chunk_raw_size == record.chunk_raw_size;
                    if !consistent {
                        // Conflicting frame for the SAME transfer id: drop.
                        return Ok(IngestEvent::Dropped);
                    }
                    if current.manifest_object_id != record.manifest_object_id {
                        // New Broadcast Instance of the SAME transfer (sender
                        // restarted with a new T / new encoding). Keep the
                        // ledger (chunk_done), drop every unfinished decoder —
                        // their object ids can never appear again — and rebind
                        // T. A verified Manifest stays valid: it is bound to
                        // manifest_hash, which is part of the semantics that
                        // just matched.
                        self.root = Some(record);
                        self.t = frame.t;
                        self.manifest_decoder = None;
                        self.manifest_meta = None;
                        self.chunk_decoder = None;
                        self.mismatch_streak = 0;
                        return Ok(IngestEvent::InstanceSwitched);
                    }
                    Ok(IngestEvent::Dropped) // duplicate ROOT
                } else {
                    // Foreign transfer: debounce; only ≥3 consistent ones
                    // re-lock. A *different* foreign transfer resets the
                    // streak — alternating streams must not evict the lock.
                    // (A foreign ROOT with a different T still counts: the
                    // re-lock resets T, so a stale T can never wedge us.)
                    if self.mismatch_transfer != Some(transfer) {
                        self.mismatch_streak = 0;
                        self.mismatch_transfer = Some(transfer);
                    }
                    self.mismatch_streak += 1;
                    if self.mismatch_streak >= MISMATCH_RELOCK_THRESHOLD {
                        self.root = None;
                        self.manifest_decoder = None;
                        self.manifest_meta = None;
                        self.manifest = None;
                        self.manifest_done = false;
                        self.chunk_decoder = None;
                        self.chunk_done.clear();
                        self.mismatch_streak = 0;
                        self.mismatch_transfer = None;
                        self.t = 0;
                        // Re-ingest this ROOT on the next frame (state now Idle).
                        Ok(IngestEvent::Relocked)
                    } else {
                        Ok(IngestEvent::RootMismatch {
                            streak: self.mismatch_streak,
                        })
                    }
                }
            }
        }
    }

    fn on_meta(&mut self, frame: Af2Frame) -> Result<IngestEvent, Af2ReceiverError> {
        let root = match &self.root {
            Some(r) => r,
            None => return Ok(IngestEvent::Dropped), // no ROOT → no decoder, no cache
        };
        let record = match ObjectMetaRecord::parse(&frame.body) {
            Ok(r) => r,
            Err(_) => return Ok(IngestEvent::Dropped),
        };
        if record.transfer_id != root.transfer() {
            return Ok(IngestEvent::Dropped);
        }
        // ④ Decode-time binding: recompute the object id and compare.
        if record.recompute_object_id() != frame.object_id {
            return Ok(IngestEvent::MetaRejected);
        }
        match record.role {
            ROLE_MANIFEST => {
                // The Manifest object is always index 0; a self-consistent
                // record with any other index could never route (the expected
                // object id binds index 0) — drop instead of wedging the
                // session on symbols that match no decoder.
                if record.object_index != 0 {
                    return Ok(IngestEvent::Dropped);
                }
                if self.manifest_done {
                    return Ok(IngestEvent::Dropped);
                }
                if let Some(prev) = &self.manifest_meta {
                    // First valid META froze the layout; later ones must match byte-for-byte.
                    if prev.encode().ok().as_ref() != record.encode().ok().as_ref() {
                        return Ok(IngestEvent::Dropped);
                    }
                    return Ok(IngestEvent::Dropped); // duplicate
                }
                // Manifest MUST be RAW and its raw_hash == ROOT.manifest_hash.
                if record.codec_id != CODEC_RAW || record.raw_hash != root.manifest_hash {
                    return Ok(IngestEvent::MetaRejected);
                }
                // ③ OTI gate (16 MiB manifest ceiling).
                let meta = object_meta_from_oti(&record.oti, 16 << 20)?;
                // Cross-check the OTI-declared symbol size against the T
                // observed on the wire: a mismatched decoder silently discards
                // every symbol (length inequality) while the frozen META makes
                // later ones look like duplicates — a wedged session that only
                // a 3-ROOT relock can break. Reject the META instead.
                if (meta.symbol_size as usize) != self.t {
                    return Ok(IngestEvent::MetaRejected);
                }
                let decoder =
                    Decoder::new(meta).map_err(|e| Af2ReceiverError::Decoder(e.to_string()))?;
                self.manifest_decoder = Some(decoder);
                self.manifest_meta = Some(record);
                Ok(IngestEvent::MetaBound {
                    role: ROLE_MANIFEST,
                    object_index: 0,
                })
            }
            ROLE_CHUNK => {
                if record.object_index >= root.chunk_count
                    || self.chunk_done.contains(&record.object_index)
                {
                    return Ok(IngestEvent::Dropped);
                }
                match &self.chunk_decoder {
                    Some((index, _, prev)) if *index == record.object_index => {
                        // Duplicate META for the live chunk: must be identical.
                        if prev.encode().ok().as_ref() != record.encode().ok().as_ref() {
                            return Ok(IngestEvent::Dropped);
                        }
                        return Ok(IngestEvent::Dropped);
                    }
                    _ => {}
                }
                // ③ OTI gate (encoded chunk ≤ 32 MiB wire ceiling).
                let meta = object_meta_from_oti(&record.oti, 32 << 20)?;
                // Same T cross-check as the manifest branch (see above).
                if (meta.symbol_size as usize) != self.t {
                    return Ok(IngestEvent::MetaRejected);
                }
                let decoder =
                    Decoder::new(meta).map_err(|e| Af2ReceiverError::Decoder(e.to_string()))?;
                let event = IngestEvent::MetaBound {
                    role: ROLE_CHUNK,
                    object_index: record.object_index,
                };
                self.chunk_decoder = Some((
                    record.object_index,
                    decoder,
                    record,
                ));
                Ok(event)
            }
            _ => Ok(IngestEvent::Dropped),
        }
    }

    fn on_symbol(&mut self, frame: Af2Frame) -> Result<IngestEvent, Af2ReceiverError> {
        if self.root.is_none() {
            return Ok(IngestEvent::Dropped);
        }
        // Unknown-object symbols: drop, zero cache (§11 resource policy).
        // Take the live slots out so `self` is free for the finish* paths.
        let mut chunk_slot = self.chunk_decoder.take();
        if let Some((index, decoder, meta)) = &mut chunk_slot {
            let expected = crate::id::object_id(
                &meta.transfer_id,
                ROLE_CHUNK,
                *index,
                meta.codec_id,
                meta.fec_id,
                &meta.oti,
                &meta.encoded_hash,
            );
            if frame.object_id == expected {
                let idx = *index;
                let ev = self.feed_symbol(decoder, meta.clone(), frame, false, idx)?;
                let finished = matches!(
                    ev,
                    IngestEvent::ChunkReady { .. } | IngestEvent::ChunkRejected
                );
                if !finished {
                    self.chunk_decoder = chunk_slot;
                }
                return Ok(ev);
            }
        }
        self.chunk_decoder = chunk_slot;

        let mut manifest_decoder = self.manifest_decoder.take();
        if let (Some(decoder), Some(expected)) = (&mut manifest_decoder, self.manifest_object_id()) {
            if frame.object_id == expected {
                let meta = match &self.manifest_meta {
                    Some(m) => m.clone(),
                    None => {
                        self.manifest_decoder = manifest_decoder;
                        return Ok(IngestEvent::Dropped);
                    }
                };
                let ev = self.feed_symbol(decoder, meta, frame, true, 0)?;
                let finished = matches!(
                    ev,
                    IngestEvent::ManifestReady | IngestEvent::ChunkRejected
                );
                if !finished {
                    self.manifest_decoder = manifest_decoder;
                }
                return Ok(ev);
            }
        }
        self.manifest_decoder = manifest_decoder;
        Ok(IngestEvent::Dropped)
    }

    fn feed_symbol(
        &mut self,
        decoder: &mut Decoder,
        meta: ObjectMetaRecord,
        frame: Af2Frame,
        is_manifest: bool,
        chunk_index: u32,
    ) -> Result<IngestEvent, Af2ReceiverError> {
        let symbol = Symbol::new(frame.sbn as u32, frame.esi, frame.body);
        let complete = match decoder.add_symbol(&symbol) {
            Ok(c) => c,
            Err(e) => {
                // The caller already took this decoder slot away. If it is
                // the manifest, also unfreeze the bound META — otherwise the
                // frozen `manifest_meta` would drop every later META as a
                // duplicate while no decoder exists (session deadlock).
                if is_manifest {
                    self.manifest_meta = None;
                }
                return Err(Af2ReceiverError::Decoder(e.to_string()));
            }
        };
        if !complete {
            return Ok(IngestEvent::SymbolAccepted);
        }
        let encoded = match decoder.assemble() {
            Some(v) => v,
            None => return Ok(IngestEvent::SymbolAccepted),
        };
        if is_manifest {
            self.finish_manifest(encoded, meta)
        } else {
            self.finish_chunk(chunk_index, encoded, meta)
        }
    }

    fn manifest_object_id(&self) -> Option<[u8; 16]> {
        let root = self.root.as_ref()?;
        let meta = self.manifest_meta.as_ref()?;
        Some(crate::id::object_id(
            &root.transfer(),
            ROLE_MANIFEST,
            0,
            meta.codec_id,
            meta.fec_id,
            &meta.oti,
            &meta.encoded_hash,
        ))
    }

    fn finish_manifest(
        &mut self,
        encoded: Vec<u8>,
        meta: ObjectMetaRecord,
    ) -> Result<IngestEvent, Af2ReceiverError> {
        let root = match &self.root {
            Some(r) => r.clone(),
            None => return Ok(IngestEvent::Dropped),
        };
        // ④ Byte-time binding: verify the encoded hash against the META.
        // Every failure path unfreezes `manifest_meta`: the decoder was
        // already consumed, so keeping the freeze would drop all future
        // META frames as duplicates with no decoder to feed (deadlock).
        if hash(&encoded) != meta.encoded_hash {
            self.manifest_meta = None;
            return Ok(IngestEvent::ChunkRejected);
        }
        // ⑦ Manifest hash (against ROOT).
        if hash(&encoded) != root.manifest_hash {
            self.manifest_meta = None;
            return Err(Af2ReceiverError::ManifestHashMismatch);
        }
        // Full manifest parse + validation (paths, stream chain, content id).
        match Manifest::parse(&encoded) {
            Ok((m, manifest_cid)) => {
                // §7 cross-check: the Manifest's carried content id must equal
                // the ROOT's (manifest_hash already bound the bytes to ROOT;
                // this binds the announced identity as well).
                if manifest_cid != root.content_id {
                    self.manifest_meta = None;
                    return Ok(IngestEvent::ChunkRejected);
                }
                self.manifest = Some(m);
                self.manifest_done = true;
                self.manifest_decoder = None;
                Ok(IngestEvent::ManifestReady)
            }
            Err(_) => {
                self.manifest_meta = None;
                Ok(IngestEvent::ChunkRejected)
            }
        }
    }

    /// Final integrity gate (§13 ⑧⑨): verify a fully reassembled Canonical
    /// Content Stream — per-entry hashes, strict UTF-8 for UTF8_TEXT entries,
    /// exact total length, and a fresh Content ID recomputation against ROOT.
    /// Hosts MUST run this before materializing/publishing files.
    pub fn verify_final_stream(&self, stream: &[u8]) -> Result<(), FinalizeError> {
        let root = self.root.as_ref().ok_or(FinalizeError::NotReady)?;
        let manifest = self.manifest.as_ref().ok_or(FinalizeError::NotReady)?;
        verify_stream(root, manifest, stream)
    }

    fn finish_chunk(
        &mut self,
        index: u32,
        encoded: Vec<u8>,
        meta: ObjectMetaRecord,
    ) -> Result<IngestEvent, Af2ReceiverError> {
        let root = match &self.root {
            Some(r) => r.clone(),
            None => return Ok(IngestEvent::Dropped),
        };
        // ④ Byte-time binding: encoded_hash from META.
        if hash(&encoded) != meta.encoded_hash {
            self.chunk_decoder = None;
            return Ok(IngestEvent::ChunkRejected);
        }
        // ⑤ Bounded decompression to the canonical chunk length.
        // u64 math: `total_raw_size as usize` truncates on wasm32 and
        // `index * chunk_raw_size` wraps there, which would corrupt the
        // expected length for any multi-chunk transfer.
        let chunk_start = u64::from(index) * u64::from(root.chunk_raw_size);
        let canonical_len = root
            .total_raw_size
            .saturating_sub(chunk_start)
            .min(u64::from(root.chunk_raw_size)) as usize;
        let raw = match decode_chunk(meta.codec_id, &encoded, canonical_len) {
            Ok(v) => v,
            Err(_) => {
                self.chunk_decoder = None;
                return Ok(IngestEvent::ChunkRejected);
            }
        };
        // ⑥ Chunk hash (against META.raw_hash; when the Manifest arrives after
        // this chunk the host re-verifies via `verify_chunk`).
        if hash(&raw) != meta.raw_hash {
            self.chunk_decoder = None;
            return Ok(IngestEvent::ChunkRejected);
        }
        // ⑥b Chunk hash against the ROOT-bound Manifest table (when it is
        // already known). The Manifest locks the chunk hashes, so a chunk that
        // decodes to different bytes must not be committed — a malicious or
        // glitched broadcast must never materialize content that contradicts
        // the Manifest it announced.
        if let Some(m) = &self.manifest {
            if m.chunk_hashes.get(index as usize) != Some(&hash(&raw)) {
                self.chunk_decoder = None;
                return Ok(IngestEvent::ChunkRejected);
            }
        }
        self.chunk_done.insert(index);
        self.chunk_decoder = None;
        Ok(IngestEvent::ChunkReady { index, raw })
    }
}

/// Standalone §13 ⑧⑨ verification: entry hashes → UTF8_TEXT strictness →
/// exact stream length → Content ID recomputation. Shared by
/// [`Af2Receiver::verify_final_stream`] and the cross-end FFI surfaces so the
/// final gate has exactly one implementation.
pub fn verify_stream(
    root: &RootRecord,
    manifest: &Manifest,
    stream: &[u8],
) -> Result<(), FinalizeError> {
    if u64::try_from(stream.len()).unwrap_or(u64::MAX) != root.total_raw_size {
        return Err(FinalizeError::Length {
            want: root.total_raw_size,
            got: stream.len() as u64,
        });
    }
    for (index, e) in manifest.entries.iter().enumerate() {
        if e.kind == KIND_DIRECTORY {
            continue;
        }
        // Checked arithmetic before slicing (wasm32 usize is 32-bit).
        let start = usize::try_from(e.content_offset).map_err(|_| FinalizeError::Length {
            want: root.total_raw_size,
            got: stream.len() as u64,
        })?;
        let end = start.checked_add(usize::try_from(e.content_size).map_err(|_| {
            FinalizeError::Length {
                want: root.total_raw_size,
                got: stream.len() as u64,
            }
        })?).ok_or(FinalizeError::Length {
            want: root.total_raw_size,
            got: stream.len() as u64,
        })?;
        if end > stream.len() || hash(&stream[start..end]) != e.content_hash {
            return Err(FinalizeError::EntryHash { index });
        }
        if e.kind == KIND_UTF8_TEXT && core::str::from_utf8(&stream[start..end]).is_err() {
            return Err(FinalizeError::NotUtf8 { index });
        }
    }
    let recomputed = content_id(
        &manifest
            .entries
            .iter()
            .map(|e| EntryIdInput {
                kind: e.kind,
                path: &e.path,
                size: if e.kind == KIND_DIRECTORY { 0 } else { e.content_size },
                entry_hash: e.content_hash,
            })
            .collect::<Vec<_>>(),
    );
    if recomputed != root.content_id {
        return Err(FinalizeError::ContentId);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::encode_chunk;
    use crate::id::{hash, KIND_UTF8_TEXT};
    use crate::manifest::build_manifest;
    use crate::meta::{CODEC_RAW, FEC_ID_RAPTORQ};

    /// Build one AF2 broadcast: ROOT + manifest object + N chunk objects.
    struct Broadcast {
        root_frame: Vec<u8>,
        manifest_meta_frame: Vec<u8>,
        manifest_symbol_frames: Vec<Vec<u8>>,
        chunk_meta_frames: Vec<Vec<u8>>,
        chunk_symbol_frames: Vec<Vec<u8>>,
        #[allow(dead_code)]
        stream: Vec<u8>,
        #[allow(dead_code)]
        manifest_object_id: [u8; 16],
        tid: [u8; 16],
        manifest_oti: [u8; 12],
        manifest_hash: [u8; 32],
    }

    fn raptorq_encode_object(
        data: &[u8],
        t: usize,
    ) -> (raptorq_core::ObjectMeta, Vec<(u8, u32, Vec<u8>)>) {
        let enc = raptorq::Encoder::with_defaults(data, t as u16);
        let oti = enc.get_config().serialize();
        let meta = object_meta_from_oti(&oti, 32 << 20).expect("valid oti");
        let mut symbols = Vec::new();
        for pkt in enc.get_encoded_packets(8) { // 8 repair packets to survive drops
            symbols.push((
                pkt.payload_id().source_block_number(),
                pkt.payload_id().encoding_symbol_id(),
                pkt.data().to_vec(),
            ));
        }
        (meta, symbols)
    }

    fn build_broadcast(data: &[u8], chunk_raw_size: u32, t: usize) -> Broadcast {
        let manifest = build_manifest(
            [(crate::id::KIND_FILE, "hello.bin", data)],
            chunk_raw_size,
        )
        .unwrap();
        let manifest_bytes = manifest.encode().unwrap();
        let manifest_hash = hash(&manifest_bytes);

        let (m_meta_obj, m_symbols) = raptorq_encode_object(&manifest_bytes, t);
        let manifest_encoded_hash = hash(&manifest_bytes);
        let tid = crate::id::transfer_id(&manifest_hash, chunk_raw_size);
        let manifest_oid = crate::id::object_id(
            &tid,
            ROLE_MANIFEST,
            0,
            CODEC_RAW,
            FEC_ID_RAPTORQ,
            &m_meta_obj.oti_bytes,
            &manifest_encoded_hash,
        );
        let root = RootRecord {
            content_id: [0; 32], // patched below via parse roundtrip
            manifest_object_id: manifest_oid,
            manifest_hash,
            total_raw_size: data.len() as u64,
            entry_count: 1,
            chunk_count: manifest.chunk_count,
            chunk_raw_size,
            extensions: vec![],
        };
        // Compute the real content id.
        let entries = manifest
            .entries
            .iter()
            .map(|e| crate::id::EntryIdInput {
                kind: e.kind,
                path: &e.path,
                size: e.content_size,
                entry_hash: e.content_hash,
            })
            .collect::<Vec<_>>();
        let root = RootRecord {
            content_id: crate::id::content_id(&entries),
            ..root
        };
        let root_frame = Af2Frame {
            frame_type: FrameType::Root,
            object_id: root.transfer(),
            sbn: 0,
            esi: 0,
            body: root.encode().unwrap(),
            t,
        }
        .to_bytes()
        .unwrap();

        let m_meta = ObjectMetaRecord {
            role: ROLE_MANIFEST,
            transfer_id: tid,
            object_index: 0,
            codec_id: CODEC_RAW,
            fec_id: FEC_ID_RAPTORQ,
            oti: m_meta_obj.oti_bytes,
            raw_hash: manifest_hash,
            encoded_hash: manifest_encoded_hash,
            extensions: vec![],
        };
        let manifest_meta_frame = Af2Frame {
            frame_type: FrameType::ObjectMeta,
            object_id: manifest_oid,
            sbn: 0,
            esi: 0,
            body: m_meta.encode().unwrap(),
            t,
        }
        .to_bytes()
        .unwrap();
        let manifest_symbol_frames = m_symbols
            .iter()
            .map(|(sbn, esi, body)| {
                Af2Frame {
                    frame_type: FrameType::Symbol,
                    object_id: manifest_oid,
                    sbn: *sbn,
                    esi: *esi,
                    body: body.clone(),
                    t,
                }
                .to_bytes()
                .unwrap()
            })
            .collect();

        // Chunks.
        let mut chunk_meta_frames = Vec::new();
        let mut chunk_symbol_frames = Vec::new();
        for i in 0..manifest.chunk_count {
            let start = i as usize * chunk_raw_size as usize;
            let end = (start + chunk_raw_size as usize).min(data.len());
            let raw = &data[start..end];
            let (codec, encoded) = encode_chunk(raw);
            let (c_meta_obj, c_symbols) = raptorq_encode_object(&encoded, t);
            let encoded_hash = hash(&encoded);
            let chunk_oid = crate::id::object_id(
                &tid,
                ROLE_CHUNK,
                i,
                codec,
                FEC_ID_RAPTORQ,
                &c_meta_obj.oti_bytes,
                &encoded_hash,
            );
            let c_meta = ObjectMetaRecord {
                role: ROLE_CHUNK,
                transfer_id: tid,
                object_index: i,
                codec_id: codec,
                fec_id: FEC_ID_RAPTORQ,
                oti: c_meta_obj.oti_bytes,
                raw_hash: hash(raw),
                encoded_hash,
                extensions: vec![],
            };
            chunk_meta_frames.push(
                Af2Frame {
                    frame_type: FrameType::ObjectMeta,
                    object_id: chunk_oid,
                    sbn: 0,
                    esi: 0,
                    body: c_meta.encode().unwrap(),
                    t,
                }
                .to_bytes()
                .unwrap(),
            );
            for (sbn, esi, body) in c_symbols {
                chunk_symbol_frames.push(
                    Af2Frame {
                        frame_type: FrameType::Symbol,
                        object_id: chunk_oid,
                        sbn,
                        esi,
                        body,
                        t,
                    }
                    .to_bytes()
                    .unwrap(),
                );
            }
        }
        Broadcast {
            root_frame,
            manifest_meta_frame,
            manifest_symbol_frames,
            chunk_meta_frames,
            chunk_symbol_frames,
            stream: data.to_vec(),
            manifest_object_id: manifest_oid,
            tid,
            manifest_oti: m_meta_obj.oti_bytes,
            manifest_hash,
        }
    }

    #[test]
    fn end_to_end_receive_with_loss_reorder_and_dupes() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let bc = build_broadcast(&data, 1 << 20, 1024); // one chunk
        let mut rx = Af2Receiver::new();

        assert_eq!(rx.ingest(&bc.root_frame).unwrap(), IngestEvent::RootLocked);

        // Symbols BEFORE any META: dropped with zero caching.
        assert_eq!(
            rx.ingest(&bc.manifest_symbol_frames[0]).unwrap(),
            IngestEvent::Dropped
        );

        assert_eq!(
            rx.ingest(&bc.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { role: ROLE_MANIFEST, object_index: 0 }
        );

        // Feed manifest symbols in reverse order, duplicating some, dropping 1 in 5.
        let mut seen_ready = false;
        for (i, f) in bc.manifest_symbol_frames.iter().enumerate().rev() {
            if i % 5 == 0 {
                continue;
            }
            let ev = rx.ingest(f).unwrap();
            if ev == IngestEvent::ManifestReady {
                seen_ready = true;
            }
            if i % 3 == 0 {
                let _ = rx.ingest(f).unwrap(); // duplicate
            }
        }
        assert!(seen_ready, "manifest must decode under loss+reorder+dupes");

        // Chunk.
        assert_eq!(
            rx.ingest(&bc.chunk_meta_frames[0]).unwrap(),
            IngestEvent::MetaBound { role: ROLE_CHUNK, object_index: 0 }
        );
        let mut chunk_ready = None;
        for f in &bc.chunk_symbol_frames {
            let ev = rx.ingest(f).unwrap();
            if let IngestEvent::ChunkReady { index, raw } = ev {
                chunk_ready = Some((index, raw));
            }
        }
        let (index, raw) = chunk_ready.expect("chunk must decode");
        assert_eq!(index, 0);
        assert_eq!(raw, data, "recovered chunk bytes must equal the original");
    }

    #[test]
    fn meta_object_id_binding_rejects_spoofed_frames() {
        let data = vec![9u8; 2000];
        let bc = build_broadcast(&data, 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        let _ = rx.ingest(&bc.root_frame).unwrap();
        // Tamper the META's frame object id: recomputation must catch it.
        let mut spoofed = bc.manifest_meta_frame.clone();
        spoofed[4] ^= 0xFF;
        // Fix the frame CRC so only the id binding fails.
        let crc = crc32fast::Hasher::new();
        let mut h = crc;
        h.update(&spoofed[..spoofed.len() - 4]);
        let fixed = h.finalize();
        let n = spoofed.len();
        spoofed[n - 4..].copy_from_slice(&fixed.to_be_bytes());
        assert_eq!(rx.ingest(&spoofed).unwrap(), IngestEvent::MetaRejected); // id mismatch w/ transfer → dropped at binding stage
    }

    #[test]
    fn mismatch_relock_requires_three_consistent_roots() {
        let data = vec![1u8; 1000];
        let bc = build_broadcast(&data, 1 << 20, 1024);
        let other = build_broadcast(&vec![2u8; 1000], 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        let _ = rx.ingest(&bc.root_frame).unwrap();
        assert_eq!(
            rx.ingest(&other.root_frame).unwrap(),
            IngestEvent::RootMismatch { streak: 1 }
        );
        assert_eq!(
            rx.ingest(&other.root_frame).unwrap(),
            IngestEvent::RootMismatch { streak: 2 }
        );
        // Data frames never trigger a re-lock.
        assert_eq!(
            rx.ingest(&other.manifest_symbol_frames[0]).unwrap(),
            IngestEvent::Dropped
        );
        assert_eq!(rx.ingest(&other.root_frame).unwrap(), IngestEvent::Relocked);
        // After the re-lock the new transfer's ROOT binds again.
        assert_eq!(rx.ingest(&other.root_frame).unwrap(), IngestEvent::RootLocked);
    }

    #[test]
    fn v1_frames_are_fail_closed_rejected() {
        let mut rx = Af2Receiver::new();
        // A v1 ET data frame (magic 'ET').
        let mut v1_bytes = vec![0u8; 84];
        v1_bytes[0] = 0x45;
        v1_bytes[1] = 0x54;
        v1_bytes[2] = 1;
        assert_eq!(rx.ingest(&v1_bytes).unwrap(), IngestEvent::Dropped);
    }

    fn reframe_with_fixed_crc(frame: &mut [u8]) {
        let mut h = crc32fast::Hasher::new();
        h.update(&frame[..frame.len() - 4]);
        let crc = h.finalize();
        let n = frame.len();
        frame[n - 4..].copy_from_slice(&crc.to_be_bytes());
    }

    #[test]
    fn alternating_foreign_roots_do_not_relock() {
        let bc = build_broadcast(&vec![1u8; 1000], 1 << 20, 1024);
        let other1 = build_broadcast(&vec![2u8; 1000], 1 << 20, 1024);
        let other2 = build_broadcast(&vec![3u8; 1000], 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        let _ = rx.ingest(&bc.root_frame).unwrap();
        // Two foreign streams alternating forever: each new one resets the
        // other's streak, so neither may ever accumulate 3 consistent ROOTs.
        for _ in 0..10 {
            let _ = rx.ingest(&other1.root_frame).unwrap();
            let _ = rx.ingest(&other2.root_frame).unwrap();
        }
        // Still locked to the original transfer: its ROOT is a consistent
        // duplicate (Dropped), not a fresh lock (RootLocked).
        assert_eq!(rx.ingest(&bc.root_frame).unwrap(), IngestEvent::Dropped);
    }

    #[test]
    fn manifest_verify_failure_unfreezes_and_allows_rebind() {
        let bc = build_broadcast(&vec![7u8; 4000], 1 << 20, 1024);
        // Corrupt EVERY manifest symbol (source + repair, CRCs fixed up) so
        // the decoder is guaranteed to complete on wrong bytes — good repair
        // symbols would otherwise heal a single corrupted source symbol.
        // The flipped byte sits inside the real manifest data area: the tail
        // of a symbol is zero padding that the decoder truncates away.
        let bad_frames: Vec<Vec<u8>> = bc
            .manifest_symbol_frames
            .iter()
            .map(|f| {
                let mut bad = f.clone();
                bad[crate::frame::HEADER_SIZE + 100] ^= 0xFF;
                reframe_with_fixed_crc(&mut bad);
                bad
            })
            .collect();
        let mut rx = Af2Receiver::new();
        let _ = rx.ingest(&bc.root_frame).unwrap();
        assert_eq!(
            rx.ingest(&bc.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { role: ROLE_MANIFEST, object_index: 0 }
        );
        for f in &bad_frames {
            let _ = rx.ingest(f).unwrap();
        }
        assert!(rx.manifest().is_none(), "corrupted manifest must not verify");
        // Before the unfreeze fix this META was dropped forever (deadlock).
        assert_eq!(
            rx.ingest(&bc.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { role: ROLE_MANIFEST, object_index: 0 }
        );
        let mut ready = false;
        for f in &bc.manifest_symbol_frames {
            if matches!(rx.ingest(f).unwrap(), IngestEvent::ManifestReady) {
                ready = true;
            }
        }
        assert!(ready, "manifest must recover after a fresh bind");
    }

    #[test]
    fn manifest_meta_with_nonzero_object_index_is_dropped() {
        let bc = build_broadcast(&vec![5u8; 1000], 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        let _ = rx.ingest(&bc.root_frame).unwrap();
        // Self-consistent MANIFEST record (binding recomputation passes)
        // carrying object_index = 1: the role forces index 0, so it must be
        // dropped — not bound to a decoder that can never be fed.
        let bad_oid = crate::id::object_id(
            &bc.tid,
            ROLE_MANIFEST,
            1,
            CODEC_RAW,
            FEC_ID_RAPTORQ,
            &bc.manifest_oti,
            &bc.manifest_hash,
        );
        let bad_meta = ObjectMetaRecord {
            role: ROLE_MANIFEST,
            transfer_id: bc.tid,
            object_index: 1,
            codec_id: CODEC_RAW,
            fec_id: FEC_ID_RAPTORQ,
            oti: bc.manifest_oti,
            raw_hash: bc.manifest_hash,
            encoded_hash: bc.manifest_hash,
            extensions: vec![],
        };
        let frame = Af2Frame {
            frame_type: FrameType::ObjectMeta,
            object_id: bad_oid,
            sbn: 0,
            esi: 0,
            body: bad_meta.encode().unwrap(),
            t: 1024,
        }
        .to_bytes()
        .unwrap();
        assert_eq!(rx.ingest(&frame).unwrap(), IngestEvent::Dropped);
    }

    #[test]
    fn instance_switch_accepts_new_broadcast_instance() {
        // §6: a same-transfer re-broadcast with a new T yields a new
        // manifest_object_id (OTI is part of the object id). The receiver must
        // accept the new instance, keep the chunk ledger, drop unfinished
        // decoders, and re-bind T — not wedge on the frozen first META.
        let data = vec![4u8; 3000];
        let bc_t1024 = build_broadcast(&data, 1 << 20, 1024);
        let bc_t2048 = build_broadcast(&data, 1 << 20, 2048);
        assert_eq!(
            bc_t1024.tid, bc_t2048.tid,
            "same manifest + chunk size ⇒ same transfer id"
        );
        let mut rx = Af2Receiver::new();
        assert_eq!(rx.ingest(&bc_t1024.root_frame).unwrap(), IngestEvent::RootLocked);
        assert_eq!(rx.symbol_size(), 1024);
        // Old-instance META binds first and freezes.
        assert!(matches!(
            rx.ingest(&bc_t1024.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { .. }
        ));
        // The new instance's ROOT switches (same semantics, new manifest oid).
        assert_eq!(
            rx.ingest(&bc_t2048.root_frame).unwrap(),
            IngestEvent::InstanceSwitched
        );
        assert_eq!(rx.symbol_size(), 2048);
        // Old-T frames are now dropped; new-instance META binds cleanly.
        assert_eq!(
            rx.ingest(&bc_t1024.manifest_meta_frame).unwrap(),
            IngestEvent::Dropped
        );
        assert!(matches!(
            rx.ingest(&bc_t2048.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { .. }
        ));
        let mut ready = false;
        for f in &bc_t2048.manifest_symbol_frames {
            if matches!(rx.ingest(f).unwrap(), IngestEvent::ManifestReady) {
                ready = true;
            }
        }
        assert!(ready, "manifest must decode from the new instance");
    }

    #[test]
    fn foreign_root_with_different_t_can_relock() {
        // A receiver locked at T=1024 must still be able to re-lock onto a
        // foreign transfer broadcasting at another T (e.g. the adjacent
        // sender changed settings). The T filter may not gate ROOT frames.
        let bc = build_broadcast(&vec![1u8; 1000], 1 << 20, 1024);
        let other = build_broadcast(&vec![2u8; 1000], 1 << 20, 2048);
        let mut rx = Af2Receiver::new();
        assert_eq!(rx.ingest(&bc.root_frame).unwrap(), IngestEvent::RootLocked);
        assert!(matches!(
            rx.ingest(&other.root_frame).unwrap(),
            IngestEvent::RootMismatch { .. }
        ));
        assert!(matches!(
            rx.ingest(&other.root_frame).unwrap(),
            IngestEvent::RootMismatch { .. }
        ));
        assert_eq!(rx.ingest(&other.root_frame).unwrap(), IngestEvent::Relocked);
        assert_eq!(rx.ingest(&other.root_frame).unwrap(), IngestEvent::RootLocked);
        assert_eq!(rx.symbol_size(), 2048, "T re-binds at the new lock");
    }

    #[test]
    fn stray_foreign_t_frame_before_lock_does_not_wedge() {
        // The very first decoded frame may be a stray symbol from a DIFFERENT
        // broadcast (another T). Locking T onto it must be impossible: T is
        // only bound at ROOT lock.
        let bc = build_broadcast(&vec![1u8; 1000], 1 << 20, 1024);
        let other = build_broadcast(&vec![2u8; 1000], 1 << 20, 2048);
        let mut rx = Af2Receiver::new();
        assert_eq!(
            rx.ingest(&other.manifest_symbol_frames[0]).unwrap(),
            IngestEvent::Dropped
        );
        assert_eq!(rx.ingest(&bc.root_frame).unwrap(), IngestEvent::RootLocked);
        assert_eq!(rx.symbol_size(), 1024);
    }

    #[test]
    fn resume_restores_ledger_and_lock() {
        let data: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let bc = build_broadcast(&data, 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        assert_eq!(rx.resume(&bc.root_frame, &[0, 7]).expect("resume"), 1, "out-of-range index ignored");
        assert_eq!(rx.symbol_size(), 1024, "T bound from the stored ROOT");
        // The ledger's completed chunk is ignored cheaply on replay.
        assert_eq!(
            rx.ingest(&bc.chunk_meta_frames[0]).unwrap(),
            IngestEvent::Dropped
        );
        // The manifest still decodes and binds.
        assert!(matches!(
            rx.ingest(&bc.manifest_meta_frame).unwrap(),
            IngestEvent::MetaBound { .. }
        ));
        let mut ready = false;
        for f in &bc.manifest_symbol_frames {
            if matches!(rx.ingest(f).unwrap(), IngestEvent::ManifestReady) {
                ready = true;
            }
        }
        assert!(ready);
        // Resuming into a live session is refused.
        assert!(rx.resume(&bc.root_frame, &[0]).is_err());
        // Resuming with a non-ROOT stored frame is refused.
        let mut rx2 = Af2Receiver::new();
        assert!(rx2.resume(&bc.manifest_meta_frame, &[]).is_err());
    }

    #[test]
    fn verify_final_stream_end_to_end() {
        // Full receive → reassemble → §13 ⑧⑨ gate passes; any tamper fails.
        // build_broadcast tags the entry UTF8_TEXT, so the payload must be
        // valid UTF-8 for the clean pass (ASCII here).
        let data: Vec<u8> = (0..5000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let bc = build_broadcast(&data, 1 << 20, 1024);
        let mut rx = Af2Receiver::new();
        rx.ingest(&bc.root_frame).unwrap();
        rx.ingest(&bc.manifest_meta_frame).unwrap();
        for f in &bc.manifest_symbol_frames {
            rx.ingest(f).unwrap();
        }
        rx.ingest(&bc.chunk_meta_frames[0]).unwrap();
        for f in &bc.chunk_symbol_frames {
            rx.ingest(f).unwrap();
        }
        // verify_final_stream needs root+manifest only; pass the exact stream.
        rx.verify_final_stream(&data).expect("clean stream must verify");
        let mut tampered = data.clone();
        tampered[0] ^= 0xFF;
        assert_eq!(
            rx.verify_final_stream(&tampered),
            Err(FinalizeError::EntryHash { index: 0 })
        );
        let short = data[..data.len() - 1].to_vec();
        assert!(matches!(
            rx.verify_final_stream(&short),
            Err(FinalizeError::Length { .. })
        ));
    }

    #[test]
    fn verify_stream_rejects_bad_utf8_text_and_wrong_content_id() {
        use crate::manifest::ManifestEntry;
        let bad = [0xFFu8, 0xFE, 0x01, 0x02]; // invalid UTF-8
        let m = Manifest {
            entries: vec![ManifestEntry {
                kind: KIND_UTF8_TEXT,
                path: "t.txt".into(),
                content_offset: 0,
                content_size: bad.len() as u64,
                content_hash: hash(&bad),
                extensions: vec![],
            }],
            chunk_count: 1,
            chunk_raw_size: 1 << 20,
            total_raw_size: bad.len() as u64,
            chunk_hashes: vec![hash(&bad)],
            extensions: vec![],
        };
        let inputs = vec![EntryIdInput {
            kind: KIND_UTF8_TEXT,
            path: "t.txt",
            size: bad.len() as u64,
            entry_hash: hash(&bad),
        }];
        let root = RootRecord {
            content_id: content_id(&inputs),
            manifest_object_id: [0; 16],
            manifest_hash: [0; 32],
            total_raw_size: bad.len() as u64,
            entry_count: 1,
            chunk_count: 1,
            chunk_raw_size: 1 << 20,
            extensions: vec![],
        };
        assert_eq!(
            verify_stream(&root, &m, &bad),
            Err(FinalizeError::NotUtf8 { index: 0 })
        );
        // Valid UTF-8 payload but ROOT announcing a different content id → ⑨ fails.
        let good_data = b"clean-text-content";
        let m_good = Manifest {
            entries: vec![ManifestEntry {
                kind: KIND_UTF8_TEXT,
                path: "t.txt".into(),
                content_offset: 0,
                content_size: good_data.len() as u64,
                content_hash: hash(good_data),
                extensions: vec![],
            }],
            chunk_count: 1,
            chunk_raw_size: 1 << 20,
            total_raw_size: good_data.len() as u64,
            chunk_hashes: vec![hash(good_data)],
            extensions: vec![],
        };
        let mut wrong = root.clone();
        wrong.total_raw_size = good_data.len() as u64;
        wrong.content_id = [0x77; 32];
        assert_eq!(
            verify_stream(&wrong, &m_good, good_data),
            Err(FinalizeError::ContentId)
        );
    }
}
