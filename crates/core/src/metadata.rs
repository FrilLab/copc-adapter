use crate::binary::{read_u16, read_u64};
use crate::error::{CopcError, Result};
use crate::header::{
    CrsWktProvenance, CrsWktSource, EvlrReadPlan, FetchedEvlr, ParsedCopcMetadata,
    copc_metadata_layout, parse_header_prefix,
};
use crate::vlr::{VlrVisit, for_each_vlr};

const EVLR_HEADER_SIZE: usize = 60;

/// Inspect one EVLR header and identify any payload range needed for WKT.
///
/// The host supplies the raw header bytes and performs the returned range read.
/// The caller feeds each returned `next_header_offset` into the next call, so
/// Rust controls traversal while no EVLR section or unrelated payload is read
/// in full.
pub fn plan_next_evlr_read(
    las_header: &[u8],
    index: u32,
    expected_header_offset: u64,
    evlr_header: &[u8],
    source_size: Option<u64>,
    fetched_wkt_bytes: u64,
    max_wkt_bytes: usize,
) -> Result<EvlrReadPlan> {
    let layout = copc_metadata_layout(las_header, usize::MAX)?;
    if source_size.is_some_and(|size| size < layout.point_data_offset as u64) {
        return Err(CopcError::new(
            "invalid-header",
            "LAS metadata prefix exceeds the source size",
        ));
    }
    if index >= layout.evlr_count {
        return Err(CopcError::new(
            "invalid-value",
            "EVLR index exceeds the declared count",
        ));
    }
    if index == 0 && expected_header_offset != layout.first_evlr_offset {
        return Err(CopcError::new(
            "invalid-header",
            "first EVLR offset does not match the LAS header",
        ));
    }
    if evlr_header.len() != EVLR_HEADER_SIZE {
        return Err(CopcError::new(
            "truncated",
            "EVLR header must contain exactly 60 bytes",
        ));
    }
    if let Some(size) = source_size {
        let header_end = expected_header_offset
            .checked_add(EVLR_HEADER_SIZE as u64)
            .ok_or_else(|| CopcError::new("overflow", "EVLR header range overflows"))?;
        if expected_header_offset > size || header_end > size {
            return Err(CopcError::new(
                "invalid-header",
                "LAS EVLR header exceeds the source size",
            ));
        }
    }

    let user_id = crate::binary::las_string(&evlr_header[2..18]);
    let record_id = read_u16(evlr_header, 18, "EVLR record ID")?;
    let payload_length = read_u64(evlr_header, 20, "EVLR record length")?;
    let payload_offset = expected_header_offset
        .checked_add(EVLR_HEADER_SIZE as u64)
        .ok_or_else(|| CopcError::new("overflow", "EVLR payload offset overflows"))?;
    let payload_end = payload_offset
        .checked_add(payload_length)
        .ok_or_else(|| CopcError::new("overflow", "EVLR payload range overflows"))?;
    if let Some(size) = source_size
        && payload_end > size
    {
        return Err(CopcError::new(
            "invalid-header",
            "LAS EVLR payload exceeds the source size",
        ));
    }

    let is_wkt = user_id == "LASF_Projection" && record_id == 2112;
    let wkt_payload_length = if is_wkt {
        let length = usize::try_from(payload_length)
            .map_err(|_| CopcError::new("unsupported-value", "CRS WKT EVLR is too large"))?;
        let total = fetched_wkt_bytes
            .checked_add(payload_length)
            .ok_or_else(|| CopcError::new("overflow", "total CRS WKT EVLR length overflows"))?;
        if length > max_wkt_bytes || total > max_wkt_bytes as u64 {
            return Err(CopcError::new(
                "unsupported-value",
                format!("CRS WKT EVLR payloads exceed the {max_wkt_bytes}-byte safety limit"),
            ));
        }
        Some(payload_length)
    } else {
        None
    };

    let next_header_offset = (index + 1 < layout.evlr_count).then_some(payload_end);
    Ok(EvlrReadPlan {
        next_header_offset,
        payload_offset,
        wkt_payload_length,
    })
}

/// Interpret a bounded LAS header/VLR prefix and selectively fetched EVLR WKT payloads.
///
/// `evlrs` must contain one raw 60-byte header per EVLR in source order. Only
/// records with user ID `LASF_Projection` and record ID 2112 carry payloads;
/// all other payloads stay remote. When known, `source_size` is checked against
/// every declared EVLR range, including payloads that were not fetched.
pub fn parse_copc_metadata(
    prefix: &[u8],
    evlrs: &[FetchedEvlr<'_>],
    source_size: Option<u64>,
    max_wkt_bytes: usize,
) -> Result<ParsedCopcMetadata> {
    let layout = copc_metadata_layout(prefix, usize::MAX)?;
    if source_size.is_some_and(|size| size < layout.point_data_offset as u64) {
        return Err(CopcError::new(
            "invalid-header",
            "LAS metadata prefix exceeds the source size",
        ));
    }
    let mut header = parse_header_prefix(prefix)?;
    let mut vlr_wkts = Vec::new();
    for_each_vlr(
        prefix,
        layout.header_size,
        layout.point_data_offset,
        read_u32_vlr_count(prefix)?,
        "VLR payload offset overflows the input",
        |user_id, record_id, payload| {
            if user_id == "LASF_Projection" && record_id == 2112 {
                vlr_wkts.push(parse_wkt(payload, "VLR")?);
            }
            Ok(VlrVisit::Continue)
        },
    )?;

    if evlrs.len() != layout.evlr_count as usize {
        return Err(CopcError::new(
            "malformed-evlr-index",
            format!(
                "LAS declares {} EVLRs but {} headers were supplied",
                layout.evlr_count,
                evlrs.len()
            ),
        ));
    }

    let mut evlr_wkts = Vec::new();
    let mut expected_offset = layout.first_evlr_offset;
    let mut total_wkt_bytes = 0u64;
    for record in evlrs {
        if record.offset != expected_offset {
            return Err(CopcError::new(
                "invalid-header",
                "EVLR headers are not in declared source order",
            ));
        }
        if record.header.len() != EVLR_HEADER_SIZE {
            return Err(CopcError::new(
                "truncated",
                "EVLR header must contain exactly 60 bytes",
            ));
        }
        let user_id = crate::binary::las_string(&record.header[2..18]);
        let record_id = read_u16(record.header, 18, "EVLR record ID")?;
        let payload_length = read_u64(record.header, 20, "EVLR record length")?;
        let payload_offset = expected_offset
            .checked_add(EVLR_HEADER_SIZE as u64)
            .ok_or_else(|| CopcError::new("overflow", "EVLR payload offset overflows"))?;
        let payload_end = payload_offset
            .checked_add(payload_length)
            .ok_or_else(|| CopcError::new("overflow", "EVLR payload range overflows"))?;
        if let Some(size) = source_size
            && (expected_offset > size || payload_end > size)
        {
            return Err(CopcError::new(
                "invalid-header",
                "LAS EVLR range exceeds the source size",
            ));
        }

        if user_id == "LASF_Projection" && record_id == 2112 {
            let payload = record.payload.ok_or_else(|| {
                CopcError::new(
                    "missing-metadata-payload",
                    "WKT EVLR payload was not fetched",
                )
            })?;
            if payload.len() as u64 != payload_length {
                return Err(CopcError::new(
                    "truncated",
                    "WKT EVLR payload length does not match its header",
                ));
            }
            total_wkt_bytes = total_wkt_bytes
                .checked_add(payload_length)
                .ok_or_else(|| CopcError::new("overflow", "total CRS WKT EVLR length overflows"))?;
            if total_wkt_bytes > max_wkt_bytes as u64 {
                return Err(CopcError::new(
                    "unsupported-value",
                    format!("CRS WKT EVLR payloads exceed the {max_wkt_bytes}-byte safety limit"),
                ));
            }
            evlr_wkts.push(parse_wkt(payload, "EVLR")?);
        } else if record.payload.is_some() {
            return Err(CopcError::new(
                "invalid-metadata-input",
                "only CRS WKT EVLR payloads may be supplied to the metadata parser",
            ));
        }
        expected_offset = payload_end;
    }

    let selected_source = if !vlr_wkts.is_empty() {
        CrsWktSource::Vlr
    } else if !evlr_wkts.is_empty() {
        CrsWktSource::Evlr
    } else {
        CrsWktSource::Unknown
    };
    let selected_value = match selected_source {
        CrsWktSource::Unknown => None,
        CrsWktSource::Vlr => vlr_wkts.first(),
        CrsWktSource::Evlr => evlr_wkts.first(),
    };
    let conflicting = selected_value.is_some_and(|selected| {
        vlr_wkts
            .iter()
            .chain(evlr_wkts.iter())
            .any(|candidate| candidate != selected)
    });
    let mut available = Vec::with_capacity(2);
    if !vlr_wkts.is_empty() {
        available.push(CrsWktSource::Vlr);
    }
    if !evlr_wkts.is_empty() {
        available.push(CrsWktSource::Evlr);
    }
    header.wkt = selected_value.filter(|value| !value.is_empty()).cloned();
    Ok(ParsedCopcMetadata {
        header,
        crs_wkt_provenance: CrsWktProvenance {
            selected: selected_source,
            available,
            conflicting,
        },
    })
}

fn parse_wkt(payload: &[u8], record_kind: &'static str) -> Result<String> {
    let end = payload
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(payload.len());
    String::from_utf8(payload[..end].to_vec()).map_err(|_| {
        CopcError::new(
            "malformed-wkt",
            format!("WKT {record_kind} is not valid UTF-8"),
        )
    })
}

fn read_u32_vlr_count(bytes: &[u8]) -> Result<usize> {
    Ok(crate::binary::read_u32(bytes, 100, "number of VLRs")? as usize)
}

#[cfg(test)]
mod tests {
    use super::{parse_copc_metadata, plan_next_evlr_read};
    use crate::header::{CrsWktSource, FetchedEvlr};

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn append_vlr(bytes: &mut Vec<u8>, user_id: &[u8], record_id: u16, payload: &[u8]) {
        let start = bytes.len();
        bytes.resize(start + 54 + payload.len(), 0);
        bytes[start + 2..start + 2 + user_id.len()].copy_from_slice(user_id);
        put_u16(bytes, start + 18, record_id);
        put_u16(bytes, start + 20, payload.len() as u16);
        bytes[start + 54..].copy_from_slice(payload);
    }

    fn prefix(wkt: Option<&str>) -> Vec<u8> {
        let header_size = 375usize;
        let mut bytes = vec![0; header_size];
        bytes[..4].copy_from_slice(b"LASF");
        bytes[24] = 1;
        bytes[25] = 4;
        put_u16(&mut bytes, 94, header_size as u16);
        put_u32(&mut bytes, 100, 1);
        bytes[104] = 6;
        put_u16(&mut bytes, 105, 30);
        put_u64(&mut bytes, 247, 7);
        put_f64(&mut bytes, 131, 0.01);
        put_f64(&mut bytes, 139, 0.01);
        put_f64(&mut bytes, 147, 0.01);
        let mut copc = [0; 160];
        put_f64(&mut copc, 24, 1.0);
        put_f64(&mut copc, 32, 0.5);
        put_u64(&mut copc, 40, 1000);
        put_u64(&mut copc, 48, 64);
        append_vlr(&mut bytes, b"copc", 1, &copc);
        if let Some(wkt) = wkt {
            let mut payload = wkt.as_bytes().to_vec();
            payload.push(0);
            append_vlr(&mut bytes, b"LASF_Projection", 2112, &payload);
            put_u32(&mut bytes, 100, 2);
        }
        let point_data_offset = bytes.len() as u32;
        put_u32(&mut bytes, 96, point_data_offset);
        bytes
    }

    fn evlr(payload: &[u8], record_id: u16) -> Vec<u8> {
        let mut bytes = vec![0; 60 + payload.len()];
        bytes[2..2 + b"LASF_Projection".len()].copy_from_slice(b"LASF_Projection");
        put_u16(&mut bytes, 18, record_id);
        put_u64(&mut bytes, 20, payload.len() as u64);
        bytes[60..].copy_from_slice(payload);
        bytes
    }

    #[test]
    fn vlr_precedence_and_conflicts_are_resolved_in_rust() {
        let prefix = prefix(Some("GEOGCS[\"VLR\"]"));
        let offset = prefix.len() as u64;
        let mut payload = b"PROJCS[\"EVLR\"]".to_vec();
        payload.push(0);
        let source_record = evlr(&payload, 2112);
        let mut metadata_prefix = prefix;
        put_u64(&mut metadata_prefix, 235, offset);
        put_u32(&mut metadata_prefix, 243, 1);
        let record = FetchedEvlr {
            offset,
            header: &source_record[..60],
            payload: Some(&source_record[60..]),
        };

        let parsed = parse_copc_metadata(
            &metadata_prefix,
            &[record],
            Some(offset + source_record.len() as u64),
            1024,
        )
        .expect("metadata should parse");
        assert_eq!(parsed.header.wkt.as_deref(), Some("GEOGCS[\"VLR\"]"));
        assert_eq!(parsed.crs_wkt_provenance.selected, CrsWktSource::Vlr);
        assert_eq!(
            parsed.crs_wkt_provenance.available,
            [CrsWktSource::Vlr, CrsWktSource::Evlr]
        );
        assert!(parsed.crs_wkt_provenance.conflicting);
    }

    #[test]
    fn superseded_wkt_vlr_does_not_override_its_replacement_evlr() {
        let mut metadata_prefix = prefix(Some("GEOGCS[\"Superseded\"]"));
        let wkt_vlr = 375 + 54 + 160;
        metadata_prefix[wkt_vlr + 2..wkt_vlr + 2 + b"LASF_Spec".len()]
            .copy_from_slice(b"LASF_Spec");
        put_u16(&mut metadata_prefix, wkt_vlr + 18, 7);
        let offset = metadata_prefix.len() as u64;
        put_u64(&mut metadata_prefix, 235, offset);
        put_u32(&mut metadata_prefix, 243, 1);
        let source_record = evlr(b"PROJCS[\"Replacement\"]\0", 2112);
        let record = FetchedEvlr {
            offset,
            header: &source_record[..60],
            payload: Some(&source_record[60..]),
        };

        let parsed = parse_copc_metadata(
            &metadata_prefix,
            &[record],
            Some(offset + source_record.len() as u64),
            1024,
        )
        .expect("a superseded VLR and active replacement EVLR are valid");
        assert_eq!(
            parsed.header.wkt.as_deref(),
            Some("PROJCS[\"Replacement\"]")
        );
        assert_eq!(parsed.crs_wkt_provenance.selected, CrsWktSource::Evlr);
        assert_eq!(parsed.crs_wkt_provenance.available, [CrsWktSource::Evlr]);
        assert!(!parsed.crs_wkt_provenance.conflicting);
    }

    #[test]
    fn duplicate_vlr_wkt_records_are_retained_as_conflict_evidence() {
        let mut metadata_prefix = prefix(Some("GEOGCS[\"First\"]"));
        let second = b"PROJCS[\"Second\"]\0";
        append_vlr(&mut metadata_prefix, b"LASF_Projection", 2112, second);
        let point_data_offset = metadata_prefix.len() as u32;
        put_u32(&mut metadata_prefix, 96, point_data_offset);
        put_u32(&mut metadata_prefix, 100, 3);

        let parsed = parse_copc_metadata(&metadata_prefix, &[], None, 1024)
            .expect("ambiguous historical duplicate WKT remains reportable");
        assert_eq!(parsed.header.wkt.as_deref(), Some("GEOGCS[\"First\"]"));
        assert_eq!(parsed.crs_wkt_provenance.selected, CrsWktSource::Vlr);
        assert_eq!(parsed.crs_wkt_provenance.available, [CrsWktSource::Vlr]);
        assert!(parsed.crs_wkt_provenance.conflicting);
    }

    #[test]
    fn duplicate_copc_info_records_fail_with_a_stable_error() {
        let mut metadata_prefix = prefix(None);
        let copc_payload = [0; 160];
        append_vlr(&mut metadata_prefix, b"copc", 1, &copc_payload);
        let point_data_offset = metadata_prefix.len() as u32;
        put_u32(&mut metadata_prefix, 96, point_data_offset);
        put_u32(&mut metadata_prefix, 100, 2);
        assert_eq!(
            parse_copc_metadata(&metadata_prefix, &[], None, 1024)
                .unwrap_err()
                .code(),
            "duplicate-record"
        );
    }

    #[test]
    fn evlr_only_duplicates_keep_first_value_and_report_conflicts() {
        let mut metadata_prefix = prefix(None);
        let first_offset = metadata_prefix.len() as u64;
        let first_payload = b"GEOGCS[\"First\"]\0";
        let second_offset = first_offset + 60 + first_payload.len() as u64;
        put_u64(&mut metadata_prefix, 235, first_offset);
        put_u32(&mut metadata_prefix, 243, 2);
        let first_source = evlr(first_payload, 2112);
        let second_source = evlr(b"PROJCS[\"Second\"]\0", 2112);
        let source_size = first_offset + first_source.len() as u64 + second_source.len() as u64;
        let first = FetchedEvlr {
            offset: first_offset,
            header: &first_source[..60],
            payload: Some(&first_source[60..]),
        };
        let second = FetchedEvlr {
            offset: second_offset,
            header: &second_source[..60],
            payload: Some(&second_source[60..]),
        };

        let parsed =
            parse_copc_metadata(&metadata_prefix, &[first, second], Some(source_size), 1024)
                .expect("metadata should parse");
        assert_eq!(parsed.header.wkt.as_deref(), Some("GEOGCS[\"First\"]"));
        assert_eq!(parsed.crs_wkt_provenance.selected, CrsWktSource::Evlr);
        assert_eq!(parsed.crs_wkt_provenance.available, [CrsWktSource::Evlr]);
        assert!(parsed.crs_wkt_provenance.conflicting);
    }

    #[test]
    fn absent_wkt_has_unknown_provenance_and_no_conflict() {
        let parsed = parse_copc_metadata(&prefix(None), &[], None, 1024)
            .expect("metadata without WKT is supported");
        assert_eq!(parsed.header.wkt, None);
        assert_eq!(parsed.crs_wkt_provenance.selected, CrsWktSource::Unknown);
        assert!(parsed.crs_wkt_provenance.available.is_empty());
        assert!(!parsed.crs_wkt_provenance.conflicting);
    }

    #[test]
    fn validates_evlr_sequence_payload_lengths_source_size_and_limits() {
        let mut prefix = prefix(None);
        let offset = prefix.len() as u64;
        put_u64(&mut prefix, 235, offset);
        put_u32(&mut prefix, 243, 1);
        let mut header = [0; 60];
        header[2..2 + b"LASF_Projection".len()].copy_from_slice(b"LASF_Projection");
        put_u16(&mut header, 18, 2112);
        put_u64(&mut header, 20, 6);

        let plan = plan_next_evlr_read(&prefix, 0, offset, &header, Some(offset + 66), 0, 10)
            .expect("WKT header should produce a selective range");
        assert_eq!(plan.payload_offset, offset + 60);
        assert_eq!(plan.wkt_payload_length, Some(6));
        assert_eq!(plan.next_header_offset, None);
        assert_eq!(
            plan_next_evlr_read(&prefix, 0, offset, &header, Some(offset + 65), 0, 10)
                .unwrap_err()
                .code(),
            "invalid-header"
        );
        assert_eq!(
            plan_next_evlr_read(&prefix, 0, offset, &header, None, 0, 5)
                .unwrap_err()
                .code(),
            "unsupported-value"
        );
        assert_eq!(
            parse_copc_metadata(
                &prefix,
                &[FetchedEvlr {
                    offset,
                    header: &header,
                    payload: Some(b"short")
                }],
                None,
                10,
            )
            .unwrap_err()
            .code(),
            "truncated"
        );
    }

    #[test]
    fn rejects_header_directory_count_mismatch_and_invalid_wkt() {
        let mut prefix = prefix(None);
        let offset = prefix.len() as u64;
        put_u64(&mut prefix, 235, offset);
        put_u32(&mut prefix, 243, 1);
        assert_eq!(
            parse_copc_metadata(&prefix, &[], None, 1024)
                .unwrap_err()
                .code(),
            "malformed-evlr-index"
        );

        let mut header = [0; 60];
        header[2..2 + b"LASF_Projection".len()].copy_from_slice(b"LASF_Projection");
        put_u16(&mut header, 18, 2112);
        put_u64(&mut header, 20, 2);
        assert_eq!(
            parse_copc_metadata(
                &prefix,
                &[FetchedEvlr {
                    offset,
                    header: &header,
                    payload: Some(&[0xff, 0])
                }],
                None,
                1024,
            )
            .unwrap_err()
            .code(),
            "malformed-wkt"
        );
    }
}
