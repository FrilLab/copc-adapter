use std::fs;
use std::path::PathBuf;

use laz::LazVlrBuilder;
use laz::record::{LayeredPointRecordCompressor, RecordCompressor};

const LAS_HEADER_SIZE: usize = 375;
const COPC_INFO_SIZE: usize = 160;
const HIERARCHY_ENTRY_SIZE: usize = 32;
const WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]]";

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn vlr(user_id: &str, record_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; 54 + payload.len()];
    let user_id_bytes = user_id.as_bytes();
    let user_id_length = user_id_bytes.len().min(16);
    bytes[2..2 + user_id_length].copy_from_slice(&user_id_bytes[..user_id_length]);
    put_u16(&mut bytes, 18, record_id);
    put_u16(&mut bytes, 20, payload.len() as u16);
    bytes[54..].copy_from_slice(payload);
    bytes
}

fn extra_bytes_descriptor() -> Vec<u8> {
    let mut descriptor = vec![0; 192];
    // LAS 1.4 Extra Bytes data_type 3 is one unsigned 16-bit scalar.
    descriptor[2] = 3;
    let name = b"survey_code";
    descriptor[4..4 + name.len()].copy_from_slice(name);
    let description = b"Independent golden U16";
    descriptor[160..160 + description.len()].copy_from_slice(description);
    descriptor
}

struct GoldenPoint {
    coordinates: [i32; 3],
    intensity: u16,
    classification: u8,
    rgb: [u16; 3],
    nir: u16,
    survey_code: u16,
    gps_time: f64,
}

fn point(point: GoldenPoint) -> Vec<u8> {
    let mut record = vec![0; 40];
    for (offset, coordinate) in [0, 4, 8].into_iter().zip(point.coordinates) {
        put_i32(&mut record, offset, coordinate);
    }
    put_u16(&mut record, 12, point.intensity);
    record[14] = 0x11; // first and only return
    record[16] = point.classification;
    put_u16(&mut record, 20, 71);
    put_f64(&mut record, 22, point.gps_time);
    for (index, color) in point.rgb.into_iter().enumerate() {
        put_u16(&mut record, 30 + index * 2, color);
    }
    put_u16(&mut record, 36, point.nir);
    put_u16(&mut record, 38, point.survey_code);
    record
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let records = [
        point(GoldenPoint {
            coordinates: [-123_060_000, 44_052_000, 2_959],
            intensity: 1_001,
            classification: 2,
            rgb: [1_000, 2_000, 3_000],
            nir: 4_000,
            survey_code: 65_000,
            gps_time: 10.0,
        }),
        point(GoldenPoint {
            coordinates: [-123_060_010, 44_052_005, 2_000],
            intensity: 2_002,
            classification: 5,
            rgb: [4_000, 5_000, 6_000],
            nir: 7_000,
            survey_code: 42,
            gps_time: 11.0,
        }),
        point(GoldenPoint {
            coordinates: [-123_059_990, 44_051_995, 3_559],
            intensity: 3_003,
            classification: 6,
            rgb: [7_000, 8_000, 9_000],
            nir: 10_000,
            survey_code: 17,
            gps_time: 12.0,
        }),
        point(GoldenPoint {
            coordinates: [-123_060_005, 44_052_010, -413],
            intensity: 4_004,
            classification: 7,
            rgb: [10_000, 11_000, 12_000],
            nir: 13_000,
            survey_code: 65_535,
            gps_time: 13.0,
        }),
    ];

    let laz_vlr = LazVlrBuilder::default()
        .with_point_format(8, 2)?
        .with_fixed_chunk_size(records.len() as u32)
        .build();
    let mut laz_vlr_payload = Vec::new();
    laz_vlr.write_to(&mut laz_vlr_payload)?;

    let mut raw_points = Vec::with_capacity(records.len() * 40);
    for record in &records {
        raw_points.extend_from_slice(record);
    }
    let mut compressor = LayeredPointRecordCompressor::new(Vec::new());
    compressor.set_fields_from(laz_vlr.items())?;
    compressor.compress_many(&raw_points)?;
    compressor.done()?;
    let compressed_points = compressor.into_inner();

    let mut copc_info = vec![0; COPC_INFO_SIZE];
    put_f64(&mut copc_info, 0, -123.060_020);
    put_f64(&mut copc_info, 8, 44.051_990);
    put_f64(&mut copc_info, 16, 90.0);
    put_f64(&mut copc_info, 24, 46.0);
    put_f64(&mut copc_info, 32, 0.000_001);

    let extra_vlr = vlr("LASF_Spec", 4, &extra_bytes_descriptor());
    let mut wkt_payload = WKT.as_bytes().to_vec();
    wkt_payload.push(0);
    let wkt_vlr = vlr("LASF_Projection", 2112, &wkt_payload);
    let copc_vlr = vlr("copc", 1, &copc_info);
    let laz_vlr = vlr("laszip encoded", 22204, &laz_vlr_payload);
    let point_data_offset =
        LAS_HEADER_SIZE + copc_vlr.len() + laz_vlr.len() + extra_vlr.len() + wkt_vlr.len();
    let root_page_offset = point_data_offset;
    let root_page_length = HIERARCHY_ENTRY_SIZE * 2;
    put_u64(&mut copc_info, 40, root_page_offset as u64);
    put_u64(&mut copc_info, 48, root_page_length as u64);
    let copc_vlr = vlr("copc", 1, &copc_info);
    let point_data_offset =
        LAS_HEADER_SIZE + copc_vlr.len() + laz_vlr.len() + extra_vlr.len() + wkt_vlr.len();
    let root_page_offset = point_data_offset;
    let child_page_length = HIERARCHY_ENTRY_SIZE;
    let child_page_offset = root_page_offset + root_page_length;
    let point_chunk_offset = child_page_offset + child_page_length;

    let mut prefix = vec![0; LAS_HEADER_SIZE];
    prefix[..4].copy_from_slice(b"LASF");
    put_u16(&mut prefix, 6, 0x0010); // WKT CRS is supplied in a VLR
    prefix[24] = 1;
    prefix[25] = 4;
    put_u16(&mut prefix, 94, LAS_HEADER_SIZE as u16);
    put_u32(&mut prefix, 96, point_data_offset as u32);
    put_u32(&mut prefix, 100, 4);
    prefix[104] = 0x88; // LAS point format 8 with LAZ compression bit
    put_u16(&mut prefix, 105, 40);
    put_u64(&mut prefix, 247, records.len() as u64);
    for (offset, value) in [(131, 0.000_001), (139, 0.000_001), (147, 0.01)] {
        put_f64(&mut prefix, offset, value);
    }
    for (offset, value) in [(155, 0.0), (163, 0.0), (171, 100.0)] {
        put_f64(&mut prefix, offset, value);
    }
    for (offset, value) in [
        (179, -123.059_990),
        (187, -123.060_010),
        (195, 44.052_010),
        (203, 44.051_995),
        (211, 135.59),
        (219, 95.87),
    ] {
        put_f64(&mut prefix, offset, value);
    }

    prefix.extend_from_slice(&copc_vlr);
    prefix.extend_from_slice(&laz_vlr);
    prefix.extend_from_slice(&extra_vlr);
    prefix.extend_from_slice(&wkt_vlr);

    let mut hierarchy = vec![0; root_page_length];
    let page_entry = &mut hierarchy[HIERARCHY_ENTRY_SIZE..];
    put_i32(page_entry, 0, 1);
    put_i32(page_entry, 4, 1);
    put_i32(page_entry, 8, 1);
    put_i32(page_entry, 12, 1);
    put_u64(page_entry, 16, child_page_offset as u64);
    put_i32(page_entry, 24, HIERARCHY_ENTRY_SIZE as i32);
    put_i32(page_entry, 28, -1);

    let mut child_hierarchy = vec![0; HIERARCHY_ENTRY_SIZE];
    put_i32(&mut child_hierarchy, 0, 1);
    put_i32(&mut child_hierarchy, 4, 1);
    put_i32(&mut child_hierarchy, 8, 1);
    put_i32(&mut child_hierarchy, 12, 1);
    put_u64(&mut child_hierarchy, 16, point_chunk_offset as u64);
    put_i32(&mut child_hierarchy, 24, compressed_points.len() as i32);
    put_i32(&mut child_hierarchy, 28, records.len() as i32);

    let mut fixture = prefix;
    fixture.extend_from_slice(&hierarchy);
    fixture.extend_from_slice(&child_hierarchy);
    fixture.extend_from_slice(&compressed_points);

    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rust-only-golden-pf8.copc.laz");
    fs::create_dir_all(output.parent().ok_or("fixture path has no parent")?)?;
    fs::write(&output, fixture)?;
    println!("Wrote {}", output.display());
    Ok(())
}
