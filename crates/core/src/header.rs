use crate::binary::{ensure_range, read_f64, read_u16, read_u32, read_u64};
use crate::error::{CopcError, Result};
use crate::extra_bytes::{ExtraBytesDescriptor, parse_extra_bytes_descriptors};
use crate::vlr::{
    VlrVisit, for_each_evlr, for_each_vlr, parse_laz_vlr_payload, validate_laz_record_length,
};
use laz::LazVlr;

const LAS_HEADER_SIZE: usize = 375;
const COPC_INFO_SIZE: usize = 160;
pub(crate) const MAX_EVLRS: u32 = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct CopcHeader {
    pub version_major: u8,
    pub version_minor: u8,
    pub point_data_record_format: u8,
    pub point_data_record_length: u16,
    pub point_count: u64,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    pub bounds: [f64; 6],
    pub cube: [f64; 6],
    pub spacing: f64,
    pub root_hierarchy_page_offset: u64,
    pub root_hierarchy_page_length: u64,
    pub wkt: Option<String>,
    pub extra_dimensions: Vec<ExtraBytesDescriptor>,
}

/// Source class selected for LAS coordinate-system WKT metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrsWktSource {
    /// No WKT record was found.
    Unknown,
    /// A VLR supplied the selected WKT.
    Vlr,
    /// An EVLR supplied the selected WKT.
    Evlr,
}

impl CrsWktSource {
    /// Stable lowercase representation used by host adapters.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Vlr => "vlr",
            Self::Evlr => "evlr",
        }
    }
}

/// Provenance retained while resolving WKT from LAS VLR and EVLR records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrsWktProvenance {
    /// Source class of the selected WKT value, if any.
    pub selected: CrsWktSource,
    /// Source classes that contained one or more WKT records, in file order.
    pub available: Vec<CrsWktSource>,
    /// Whether any available WKT value differs from the selected value.
    pub conflicting: bool,
}

/// Header and CRS provenance produced by the authoritative metadata parser.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCopcMetadata {
    /// Validated LAS/COPC fields and Extra Bytes descriptors.
    pub header: CopcHeader,
    /// WKT selection and conflict evidence from VLRs and EVLRs.
    pub crs_wkt_provenance: CrsWktProvenance,
}

/// One EVLR header fetched by the host adapter, with its actual source offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEvlr<'a> {
    /// Absolute offset of this EVLR header in the LAS source.
    pub offset: u64,
    /// Raw 60-byte EVLR header as fetched from the source.
    pub header: &'a [u8],
    /// Selectively fetched payload. Present only for WKT EVLR records.
    pub payload: Option<&'a [u8]>,
}

/// Layout information used by a host adapter to fetch bounded metadata ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopcMetadataLayout {
    /// LAS public header length, including any declared extension bytes.
    pub header_size: usize,
    /// Offset to point records and end of the VLR prefix.
    pub point_data_offset: usize,
    /// Absolute offset of the first EVLR, or zero when there are none.
    pub first_evlr_offset: u64,
    /// Number of EVLR headers declared by LAS 1.4.
    pub evlr_count: u32,
}

/// Result of inspecting one raw EVLR header before selectively fetching payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvlrReadPlan {
    /// Offset for the next EVLR header, if one is declared.
    pub next_header_offset: Option<u64>,
    /// Absolute offset of this EVLR's payload.
    pub payload_offset: u64,
    /// Payload size to fetch when this record contains WKT; otherwise `None`.
    pub wkt_payload_length: Option<u64>,
}

struct CopcInfo {
    center: [f64; 3],
    half_size: f64,
    spacing: f64,
    root_offset: u64,
    root_length: u64,
}

/// Parse LAS/COPC header and VLR metadata without following remote EVLR ranges.
///
/// This function accepts a bounded prefix ending at `point_data_offset`. EVLR
/// headers and selectively fetched EVLR payloads are handled by
/// [`crate::parse_copc_metadata`].
pub fn parse_header_prefix(bytes: &[u8]) -> Result<CopcHeader> {
    ensure_range(bytes, 0, LAS_HEADER_SIZE, "LAS 1.4 header")?;
    if &bytes[0..4] != b"LASF" {
        return Err(CopcError::new(
            "invalid-header",
            "LAS signature is not LASF",
        ));
    }

    let version_major = bytes[24];
    let version_minor = bytes[25];
    if version_major != 1 || version_minor != 4 {
        return Err(CopcError::new(
            "unsupported-value",
            format!("COPC requires LAS 1.4, found LAS {version_major}.{version_minor}"),
        ));
    }

    let header_size = read_u16(bytes, 94, "header size")? as usize;
    if header_size < LAS_HEADER_SIZE {
        return Err(CopcError::new(
            "invalid-header",
            format!("LAS 1.4 header size {header_size} is smaller than {LAS_HEADER_SIZE}"),
        ));
    }
    ensure_range(bytes, 0, header_size, "declared LAS header")?;

    let point_data_offset = read_u32(bytes, 96, "point data offset")? as usize;
    let number_of_vlrs = read_u32(bytes, 100, "number of VLRs")? as usize;
    if point_data_offset < header_size {
        return Err(CopcError::new(
            "invalid-header",
            "point data offset precedes the LAS header",
        ));
    }
    ensure_range(bytes, 0, point_data_offset, "LAS VLR area")?;

    let scale = [
        read_f64(bytes, 131, "X scale")?,
        read_f64(bytes, 139, "Y scale")?,
        read_f64(bytes, 147, "Z scale")?,
    ];
    let offset = [
        read_f64(bytes, 155, "X offset")?,
        read_f64(bytes, 163, "Y offset")?,
        read_f64(bytes, 171, "Z offset")?,
    ];
    let bounds = [
        read_f64(bytes, 187, "minimum X")?,
        read_f64(bytes, 203, "minimum Y")?,
        read_f64(bytes, 219, "minimum Z")?,
        read_f64(bytes, 179, "maximum X")?,
        read_f64(bytes, 195, "maximum Y")?,
        read_f64(bytes, 211, "maximum Z")?,
    ];
    let point_count = read_u64(bytes, 247, "extended point count")?;

    let point_data_record_format = bytes[104] & 0x0f;
    let point_data_record_length = read_u16(bytes, 105, "point data record length")?;
    let base_record_length = match point_data_record_format {
        0 => Some(20),
        1 => Some(28),
        2 => Some(26),
        3 => Some(34),
        4 => Some(57),
        5 => Some(63),
        6 => Some(30),
        7 => Some(36),
        8 => Some(38),
        9 => Some(59),
        10 => Some(67),
        _ => None,
    };
    let mut copc_info: Option<CopcInfo> = None;
    let mut wkt: Option<String> = None;
    let mut wkt_record_found = false;
    let mut copc_info_found = false;
    let mut laz_vlr_found = false;
    let mut extra_bytes_payloads: Vec<Vec<u8>> = Vec::new();

    for_each_vlr(
        bytes,
        header_size,
        point_data_offset,
        number_of_vlrs,
        "VLR payload offset overflows the input",
        |record_user_id, record_id, payload| {
            if record_user_id == "copc" && record_id == 1 {
                if copc_info_found {
                    return Err(CopcError::new(
                        "duplicate-record",
                        "COPC info VLR occurs more than once",
                    ));
                }
                copc_info_found = true;
                if payload.len() != COPC_INFO_SIZE {
                    return Err(CopcError::new(
                        "malformed-copc-info",
                        format!(
                            "COPC info VLR has {} bytes; expected {COPC_INFO_SIZE}",
                            payload.len()
                        ),
                    ));
                }
                copc_info = Some(CopcInfo {
                    center: [
                        read_f64(payload, 0, "COPC center X")?,
                        read_f64(payload, 8, "COPC center Y")?,
                        read_f64(payload, 16, "COPC center Z")?,
                    ],
                    half_size: read_f64(payload, 24, "COPC half-size")?,
                    spacing: read_f64(payload, 32, "COPC spacing")?,
                    root_offset: read_u64(payload, 40, "root hierarchy page offset")?,
                    root_length: read_u64(payload, 48, "root hierarchy page length")?,
                });
            }

            if record_user_id == LazVlr::USER_ID && record_id == LazVlr::RECORD_ID {
                if laz_vlr_found {
                    return Err(CopcError::new(
                        "duplicate-record",
                        "LASZIP VLR occurs more than once",
                    ));
                }
                laz_vlr_found = true;
                if base_record_length
                    .is_some_and(|minimum| usize::from(point_data_record_length) < minimum)
                {
                    return Err(CopcError::new(
                        "unsupported-point-format",
                        "LAS point record is shorter than its format-defined fields",
                    ));
                }
                let laz_vlr = parse_laz_vlr_payload(payload)?;
                if matches!(point_data_record_format, 6..=8) {
                    validate_laz_record_length(&laz_vlr, point_data_record_length)?;
                }
            }

            if record_user_id == "LASF_Projection" && record_id == 2112 {
                if wkt_record_found {
                    return Ok(VlrVisit::Continue);
                }
                wkt_record_found = true;
                let end = payload
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(payload.len());
                let value = String::from_utf8(payload[..end].to_vec())
                    .map_err(|_| CopcError::new("malformed-wkt", "WKT VLR is not valid UTF-8"))?;
                if !value.is_empty() {
                    wkt = Some(value);
                }
            }

            if record_user_id == "LASF_Spec" && record_id == 4 {
                extra_bytes_payloads.push(payload.to_vec());
            }

            Ok(VlrVisit::Continue)
        },
    )?;

    let copc_info = copc_info.ok_or_else(|| {
        CopcError::new(
            "missing-copc-info",
            "COPC info VLR (user ID copc, record ID 1) was not found",
        )
    })?;
    let extra_dimensions = if extra_bytes_payloads.is_empty() {
        Vec::new()
    } else {
        let base_record_length = base_record_length.ok_or_else(|| {
            CopcError::new(
                "unsupported-point-format",
                format!(
                    "LAS point format {point_data_record_format} has no known base record size"
                ),
            )
        })?;
        let extra_record_length = usize::from(point_data_record_length)
            .checked_sub(base_record_length)
            .ok_or_else(|| {
                CopcError::new(
                    "malformed-extra-bytes",
                    "LAS point record is shorter than its format-defined fields",
                )
            })?;
        let payloads = extra_bytes_payloads
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        parse_extra_bytes_descriptors(&payloads, extra_record_length)?
    };
    let root_offset = copc_info.root_offset;
    let root_length = copc_info.root_length;
    if root_length == 0 {
        return Err(CopcError::new(
            "invalid-value",
            "root hierarchy page length must be positive",
        ));
    }
    root_offset
        .checked_add(root_length)
        .ok_or_else(|| CopcError::new("overflow", "root hierarchy page range overflows"))?;
    let center = copc_info.center;
    let half_size = copc_info.half_size;
    let spacing = copc_info.spacing;
    if half_size <= 0.0 || spacing <= 0.0 {
        return Err(CopcError::new(
            "invalid-value",
            "COPC half-size and spacing must be positive",
        ));
    }

    let cube = [
        center[0] - half_size,
        center[1] - half_size,
        center[2] - half_size,
        center[0] + half_size,
        center[1] + half_size,
        center[2] + half_size,
    ];
    if cube.iter().any(|value| !value.is_finite()) {
        return Err(CopcError::new(
            "invalid-value",
            "COPC cube bounds are not finite",
        ));
    }

    Ok(CopcHeader {
        version_major,
        version_minor,
        point_data_record_format,
        point_data_record_length,
        point_count,
        scale,
        offset,
        bounds,
        cube,
        spacing,
        root_hierarchy_page_offset: root_offset,
        root_hierarchy_page_length: root_length,
        wkt,
        extra_dimensions,
    })
}

/// Parse a complete in-memory LAS/COPC source, including all EVLR records.
///
/// Range-based readers should use [`parse_header_prefix`] and
/// [`crate::parse_copc_metadata`] so they never need to load the EVLR section
/// as a single buffer.
pub fn parse_header(bytes: &[u8]) -> Result<CopcHeader> {
    let header = parse_header_prefix(bytes)?;
    let layout = copc_metadata_layout(bytes, usize::MAX)?;
    if layout.evlr_count == 0 {
        return Ok(header);
    }

    let mut records = Vec::with_capacity(layout.evlr_count as usize);
    let mut position = layout.first_evlr_offset;
    for_each_evlr(
        bytes,
        usize::try_from(position)
            .map_err(|_| CopcError::new("unsupported-value", "EVLR offset is too large"))?,
        layout.evlr_count as usize,
        |user_id, record_id, payload| {
            let start = usize::try_from(position)
                .map_err(|_| CopcError::new("unsupported-value", "EVLR offset is too large"))?;
            let header_end = start
                .checked_add(60)
                .ok_or_else(|| CopcError::new("overflow", "EVLR header range overflows"))?;
            ensure_range(bytes, start, 60, "EVLR header")?;
            let header = &bytes[start..header_end];
            records.push(FetchedEvlr {
                offset: position,
                header,
                payload: if user_id == "LASF_Projection" && record_id == 2112 {
                    Some(payload)
                } else {
                    None
                },
            });
            position = position
                .checked_add(60)
                .and_then(|value| value.checked_add(payload.len() as u64))
                .ok_or_else(|| CopcError::new("overflow", "EVLR payload end overflows"))?;
            Ok(VlrVisit::Continue)
        },
    )?;
    crate::parse_copc_metadata(bytes, &records, None, usize::MAX).map(|parsed| parsed.header)
}

/// Read bounded LAS metadata layout fields from an initial header prefix.
pub fn copc_metadata_layout(bytes: &[u8], max_prefix_bytes: usize) -> Result<CopcMetadataLayout> {
    ensure_range(bytes, 0, LAS_HEADER_SIZE, "LAS 1.4 header")?;
    if &bytes[0..4] != b"LASF" || bytes[24] != 1 || bytes[25] != 4 {
        return Err(CopcError::new(
            "invalid-header",
            "input is not a LAS 1.4 header",
        ));
    }
    let header_size = usize::from(read_u16(bytes, 94, "header size")?);
    let point_data_offset = read_u32(bytes, 96, "point data offset")? as usize;
    if header_size < LAS_HEADER_SIZE || point_data_offset < header_size {
        return Err(CopcError::new(
            "invalid-header",
            "LAS header or VLR offset is invalid",
        ));
    }
    if point_data_offset > max_prefix_bytes {
        return Err(CopcError::new(
            "unsupported-value",
            "LAS metadata prefix exceeds the configured byte limit",
        ));
    }
    let first_evlr_offset = read_u64(bytes, 235, "first EVLR offset")?;
    let evlr_count = read_u32(bytes, 243, "number of EVLRs")?;
    if evlr_count > MAX_EVLRS {
        return Err(CopcError::new(
            "unsupported-value",
            format!("LAS EVLR count exceeds the {MAX_EVLRS}-record safety limit"),
        ));
    }
    if evlr_count > 0 && first_evlr_offset == 0 {
        return Err(CopcError::new(
            "invalid-header",
            "LAS declares EVLRs without a valid first EVLR offset",
        ));
    }
    if evlr_count > 0 && first_evlr_offset < point_data_offset as u64 {
        return Err(CopcError::new(
            "invalid-header",
            "first EVLR offset precedes the LAS point data area",
        ));
    }
    Ok(CopcMetadataLayout {
        header_size,
        point_data_offset,
        first_evlr_offset,
        evlr_count,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_header;

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn append_wkt_vlr(bytes: Vec<u8>, wkt: &str) -> Vec<u8> {
        let original_end = bytes.len();
        let mut payload = wkt.as_bytes().to_vec();
        payload.push(0);
        let mut result = vec![0; original_end + 54 + payload.len()];
        result[..original_end].copy_from_slice(&bytes);
        result[original_end + 2..original_end + 2 + b"LASF_Projection".len()]
            .copy_from_slice(b"LASF_Projection");
        put_u16(&mut result, original_end + 18, 2112);
        put_u16(&mut result, original_end + 20, payload.len() as u16);
        result[original_end + 54..].copy_from_slice(&payload);
        let point_data_offset = result.len() as u32;
        put_u32(&mut result, 96, point_data_offset);
        put_u32(&mut result, 100, 2);
        result
    }

    fn append_wkt_evlr(mut bytes: Vec<u8>, wkt: &str) -> Vec<u8> {
        let offset = bytes.len();
        let mut payload = wkt.as_bytes().to_vec();
        payload.push(0);
        bytes.resize(offset + 60 + payload.len(), 0);
        bytes[offset + 2..offset + 2 + b"LASF_Projection".len()]
            .copy_from_slice(b"LASF_Projection");
        put_u16(&mut bytes, offset + 18, 2112);
        put_u64(&mut bytes, offset + 20, payload.len() as u64);
        bytes[offset + 60..].copy_from_slice(&payload);
        put_u64(&mut bytes, 235, offset as u64);
        put_u32(&mut bytes, 243, 1);
        bytes
    }

    fn fixture_header(copc_length: u16) -> Vec<u8> {
        let header_size = 375usize;
        let copc_length = usize::from(copc_length);
        let point_data_offset = header_size + 54 + copc_length;
        let mut bytes = vec![0; point_data_offset];
        bytes[0..4].copy_from_slice(b"LASF");
        bytes[24] = 1;
        bytes[25] = 4;
        put_u16(&mut bytes, 94, header_size as u16);
        put_u32(&mut bytes, 96, point_data_offset as u32);
        put_u32(&mut bytes, 100, 1);
        bytes[104] = 0x86;
        put_u16(&mut bytes, 105, 30);
        put_f64(&mut bytes, 131, 0.01);
        put_f64(&mut bytes, 139, 0.02);
        put_f64(&mut bytes, 147, 0.03);
        put_f64(&mut bytes, 155, 10.0);
        put_f64(&mut bytes, 163, 20.0);
        put_f64(&mut bytes, 171, 30.0);
        put_f64(&mut bytes, 179, 100.0);
        put_f64(&mut bytes, 187, 1.0);
        put_f64(&mut bytes, 195, 200.0);
        put_f64(&mut bytes, 203, 2.0);
        put_f64(&mut bytes, 211, 300.0);
        put_f64(&mut bytes, 219, 3.0);
        put_u64(&mut bytes, 247, 42);

        let vlr = header_size;
        bytes[vlr + 2..vlr + 6].copy_from_slice(b"copc");
        put_u16(&mut bytes, vlr + 18, 1);
        put_u16(&mut bytes, vlr + 20, copc_length as u16);
        if copc_length == 160 {
            let payload = vlr + 54;
            put_f64(&mut bytes, payload, 50.0);
            put_f64(&mut bytes, payload + 8, 60.0);
            put_f64(&mut bytes, payload + 16, 70.0);
            put_f64(&mut bytes, payload + 24, 10.0);
            put_f64(&mut bytes, payload + 32, 2.5);
            put_u64(&mut bytes, payload + 40, 999);
            put_u64(&mut bytes, payload + 48, 64);
        }
        bytes
    }

    #[test]
    fn reads_wkt_from_evlr_when_no_vlr_wkt_exists() {
        let bytes = append_wkt_evlr(fixture_header(160), "GEOGCS[\"EVLR CRS\"]");
        let header = parse_header(&bytes).unwrap();
        assert_eq!(header.wkt.as_deref(), Some("GEOGCS[\"EVLR CRS\"]"));
    }

    #[test]
    fn gives_wkt_vlr_precedence_over_a_conflicting_evlr() {
        let bytes = append_wkt_evlr(
            append_wkt_vlr(fixture_header(160), "GEOGCS[\"VLR CRS\"]"),
            "PROJCS[\"EVLR CRS\"]",
        );
        let header = parse_header(&bytes).unwrap();
        assert_eq!(header.wkt.as_deref(), Some("GEOGCS[\"VLR CRS\"]"));
    }

    #[test]
    fn parses_header_and_copc_info() {
        let result = parse_header(&fixture_header(160)).expect("fixture should parse");
        assert_eq!(result.point_count, 42);
        assert_eq!(result.root_hierarchy_page_offset, 999);
        assert_eq!(result.cube, [40.0, 50.0, 60.0, 60.0, 70.0, 80.0]);
    }

    #[test]
    fn rejects_truncated_and_invalid_copc_headers() {
        assert_eq!(parse_header(&[0; 374]).unwrap_err().code(), "truncated");
        let missing = vec![0; 375];
        assert_eq!(parse_header(&missing).unwrap_err().code(), "invalid-header");
        assert_eq!(
            parse_header(&fixture_header(8)).unwrap_err().code(),
            "malformed-copc-info"
        );

        let mut overflow = fixture_header(160);
        put_u64(&mut overflow, 375 + 54 + 40, u64::MAX);
        assert_eq!(parse_header(&overflow).unwrap_err().code(), "overflow");
    }

    #[test]
    fn preserves_u64_domain_offsets_for_native_consumers() {
        let mut bytes = fixture_header(160);
        put_u64(&mut bytes, 375 + 54 + 40, u64::from(u32::MAX) + 1);
        put_u64(&mut bytes, 375 + 54 + 48, 64);
        let result = parse_header(&bytes).expect("native core accepts a valid u64 offset");
        assert_eq!(result.root_hierarchy_page_offset, u64::from(u32::MAX) + 1);
    }
}
