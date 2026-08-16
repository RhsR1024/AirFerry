//! AF2 four-scope TLV parser (protocol 2).
//!
//! ```text
//! type:u16 || length:u16 || value          type & 0x8000 = Critical
//! ```
//!
//! Rules (§14): types strictly ascending within one scope; no duplicates;
//! unknown Optional → skip, unknown Critical → reject the enclosing structure
//! (fail-closed "upgrade needed"); `0x4000–0x7FFF` / `0xC000–0xFFFF` are
//! experimental/vendor ranges. All length arithmetic is checked before slicing.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tlv {
    pub type_id: u16,
    pub value: Vec<u8>,
}

impl Tlv {
    pub fn is_critical(&self) -> bool {
        self.type_id & 0x8000 != 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlvError {
    #[error("truncated TLV header at {offset}")]
    TruncatedHeader { offset: usize },
    #[error("TLV value runs past the scope (offset {offset}, len {len})")]
    ValueOverflow { offset: usize, len: usize },
    #[error("TLV types must be strictly ascending (saw {prev:?} then {got:?})")]
    NotAscending { prev: u16, got: u16 },
    #[error("unknown critical TLV type 0x{type_id:04X} — receiver too old, upgrade needed")]
    UnknownCritical { type_id: u16 },
}

/// Known Optional Entry TLV types (§7.2). None are Critical in v1 of AF2.
pub const TLV_MTIME_MS: u16 = 0x0101;
pub const TLV_UNIX_MODE: u16 = 0x0102;
pub const TLV_MIME: u16 = 0x0103;
pub const TLV_TYPE_CLASS: u16 = 0x0104;

/// Encode one scope's TLV list (must already be ascending + unique).
pub fn encode_tlvs(tlvs: &[Tlv]) -> Vec<u8> {
    let mut out = Vec::new();
    for t in tlvs {
        out.extend_from_slice(&t.type_id.to_be_bytes());
        out.extend_from_slice(&(t.value.len() as u16).to_be_bytes());
        out.extend_from_slice(&t.value);
    }
    out
}

/// Parse one scope's TLV area. `allow_unknown_critical = false` for the
/// product receiver: an unknown Critical type rejects the whole structure.
pub fn parse_tlvs(bytes: &[u8]) -> Result<Vec<Tlv>, TlvError> {
    let mut out = Vec::new();
    let mut off = 0usize;
    let mut prev_type: Option<u16> = None;
    while off < bytes.len() {
        if bytes.len() - off < 4 {
            return Err(TlvError::TruncatedHeader { offset: off });
        }
        let type_id = u16::from_be_bytes([bytes[off], bytes[off + 1]]);
        let len = u16::from_be_bytes([bytes[off + 2], bytes[off + 3]]) as usize;
        off += 4;
        if bytes.len() - off < len {
            return Err(TlvError::ValueOverflow { offset: off, len });
        }
        if let Some(prev) = prev_type {
            if type_id <= prev {
                return Err(TlvError::NotAscending { prev, got: type_id });
            }
        }
        let value = bytes[off..off + len].to_vec();
        off += len;
        prev_type = Some(type_id);
        out.push(Tlv { type_id, value });
    }
    Ok(out)
}

/// Receiver-side gate: fail-closed on any unknown Critical TLV.
pub fn check_unknown_critical(tlvs: &[Tlv]) -> Result<(), TlvError> {
    // Known types today: the four Entry annotations (all Optional). ROOT /
    // OBJECT_META / Manifest scopes have no defined TLVs yet, so ANY Critical
    // type is unknown there and rejects the structure.
    const KNOWN: [u16; 4] = [TLV_MTIME_MS, TLV_UNIX_MODE, TLV_MIME, TLV_TYPE_CLASS];
    for t in tlvs {
        if t.is_critical() && !KNOWN.contains(&t.type_id) {
            return Err(TlvError::UnknownCritical { type_id: t.type_id });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let tlvs = vec![
            Tlv { type_id: TLV_MTIME_MS, value: 12345u64.to_be_bytes().to_vec() },
            Tlv { type_id: TLV_MIME, value: b"text/plain".to_vec() },
        ];
        let bytes = encode_tlvs(&tlvs);
        assert_eq!(parse_tlvs(&bytes).unwrap(), tlvs);
    }

    #[test]
    fn rejects_descending_and_duplicate_and_truncated() {
        let bad = encode_tlvs(&[
            Tlv { type_id: TLV_MIME, value: vec![] },
            Tlv { type_id: TLV_MTIME_MS, value: vec![] }, // descending
        ]);
        assert!(matches!(
            parse_tlvs(&bad),
            Err(TlvError::NotAscending { .. })
        ));
        let dup = encode_tlvs(&[
            Tlv { type_id: TLV_MIME, value: vec![] },
            Tlv { type_id: TLV_MIME, value: vec![] },
        ]);
        assert!(matches!(parse_tlvs(&dup), Err(TlvError::NotAscending { .. })));
        let truncated = &encode_tlvs(&[Tlv { type_id: TLV_MIME, value: vec![1, 2, 3] }])[..5];
        assert!(matches!(
            parse_tlvs(truncated),
            Err(TlvError::TruncatedHeader { .. }) | Err(TlvError::ValueOverflow { .. })
        ));
    }

    #[test]
    fn unknown_critical_fail_closed() {
        let tlvs = vec![Tlv { type_id: 0x8001, value: vec![] }];
        assert!(matches!(
            check_unknown_critical(&tlvs),
            Err(TlvError::UnknownCritical { .. })
        ));
        // Known optional types pass.
        assert!(check_unknown_critical(&[
            Tlv { type_id: TLV_MTIME_MS, value: vec![] }
        ])
        .is_ok());
    }
}
