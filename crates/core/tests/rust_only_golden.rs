use copc_adapter_core::{
    CopcNodePreparer, CrsWktSource, DecodedExtraDimensionValues, FIELD_CLASSIFICATION,
    FIELD_INTENSITY, FIELD_RGB, copc_metadata_layout, parse_copc_metadata, parse_root_hierarchy,
};

const GOLDEN_COPC: &[u8] = include_bytes!("fixtures/rust-only-golden-pf8.copc.laz");
const HIERARCHY_ENTRY_SIZE: usize = 32;
const HEIGHT_OFFSET_METERS: f64 = 1.25;
const WGS84_A: f64 = 6_378_137.0;
const WGS84_E2: f64 = 6.694_379_990_141_316_5e-3;

fn assert_near(actual: f64, expected: f64, tolerance: f64, label: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{label}: got {actual:.12}, expected {expected:.12} ± {tolerance}"
    );
}

fn wgs84_ecef(longitude_degrees: f64, latitude_degrees: f64, height_meters: f64) -> [f64; 3] {
    let longitude = longitude_degrees.to_radians();
    let latitude = latitude_degrees.to_radians();
    let sin_latitude = latitude.sin();
    let prime_vertical_radius = WGS84_A / (1.0 - WGS84_E2 * sin_latitude.powi(2)).sqrt();
    [
        (prime_vertical_radius + height_meters) * latitude.cos() * longitude.cos(),
        (prime_vertical_radius + height_meters) * latitude.cos() * longitude.sin(),
        (prime_vertical_radius * (1.0 - WGS84_E2) + height_meters) * sin_latitude,
    ]
}

#[test]
fn rust_only_golden_fixture_matches_spec_values_through_decode_and_preparation() {
    let layout = copc_metadata_layout(GOLDEN_COPC, 1_048_576)
        .expect("the checked-in LAS 1.4 prefix has a valid bounded VLR area");
    assert!(layout.point_data_offset < GOLDEN_COPC.len());

    let metadata = parse_copc_metadata(
        &GOLDEN_COPC[..layout.point_data_offset],
        &[],
        Some(GOLDEN_COPC.len() as u64),
        16_384,
    )
    .expect("the independent golden header and VLRs must parse");
    let header = &metadata.header;
    assert_eq!(header.version_major, 1);
    assert_eq!(header.version_minor, 4);
    assert_eq!(header.point_data_record_format, 8);
    assert_eq!(header.point_data_record_length, 40);
    assert_eq!(header.point_count, 4);
    assert_eq!(header.scale, [0.000_001, 0.000_001, 0.01]);
    assert_eq!(header.offset, [0.0, 0.0, 100.0]);
    assert_eq!(
        header.bounds,
        [
            -123.060_010,
            44.051_995,
            95.87,
            -123.059_990,
            44.052_010,
            135.59
        ]
    );
    assert!(header.cube[0] <= header.bounds[0] && header.cube[3] >= header.bounds[3]);
    assert!(header.cube[1] <= header.bounds[1] && header.cube[4] >= header.bounds[4]);
    assert!(header.cube[2] <= header.bounds[2] && header.cube[5] >= header.bounds[5]);
    assert_eq!(header.extra_dimensions.len(), 1);
    assert_eq!(header.extra_dimensions[0].name, "survey_code");
    assert_eq!(header.extra_dimensions[0].data_type, 3);
    assert_eq!(metadata.crs_wkt_provenance.selected, CrsWktSource::Vlr);
    assert_eq!(metadata.crs_wkt_provenance.available, [CrsWktSource::Vlr]);
    assert!(!metadata.crs_wkt_provenance.conflicting);
    assert!(
        header
            .wkt
            .as_deref()
            .is_some_and(|wkt| wkt.starts_with("GEOGCS[\"WGS 84\""))
    );

    let root_start = usize::try_from(header.root_hierarchy_page_offset)
        .expect("fixture root page offset fits the host address space");
    let root_length = usize::try_from(header.root_hierarchy_page_length)
        .expect("fixture root page length fits the host address space");
    let root_end = root_start
        .checked_add(root_length)
        .expect("fixture root page end does not overflow");
    let root = parse_root_hierarchy(&GOLDEN_COPC[root_start..root_end])
        .expect("the fixture root hierarchy page follows the COPC entry layout");
    assert_eq!(root.entry_count, 2);
    assert_eq!(root.nodes.len(), 1);
    assert_eq!(root.nodes[0].point_count, 0);
    assert_eq!(root.pages.len(), 1);
    let child_page = &root.pages[0];
    assert_eq!(
        (child_page.level, child_page.x, child_page.y, child_page.z),
        (1, 1, 1, 1)
    );
    assert_eq!(child_page.page_length, HIERARCHY_ENTRY_SIZE as u32);
    let child_start = usize::try_from(child_page.page_offset)
        .expect("fixture child hierarchy offset fits the host address space");
    let child_end = child_start
        .checked_add(child_page.page_length as usize)
        .expect("fixture child hierarchy end does not overflow");
    let child = parse_root_hierarchy(&GOLDEN_COPC[child_start..child_end])
        .expect("the fixture child hierarchy page follows the COPC entry layout");
    assert_eq!(child.entry_count, 1);
    assert!(child.pages.is_empty());
    assert_eq!(child.nodes.len(), 1);
    let node = &child.nodes[0];
    assert_eq!((node.level, node.x, node.y, node.z), (1, 1, 1, 1));
    assert_eq!(node.point_count, 4);
    assert_eq!(node.point_data_length, 208);

    let chunk_start = usize::try_from(node.point_data_offset)
        .expect("fixture point chunk offset fits the host address space");
    let chunk_end = chunk_start
        .checked_add(node.point_data_length as usize)
        .expect("fixture point chunk end does not overflow");
    let chunk = &GOLDEN_COPC[chunk_start..chunk_end];
    let preparer = CopcNodePreparer::from_metadata_with_height_offset(
        &GOLDEN_COPC[..layout.point_data_offset],
        64 * 1024 * 1024,
        HEIGHT_OFFSET_METERS,
    )
    .expect("the Rust core initializes its LASzip and WGS 84 transforms");
    let prepared = preparer
        .prepare_node_with_extra_dimensions(
            chunk,
            node.point_count as usize,
            FIELD_INTENSITY | FIELD_CLASSIFICATION | FIELD_RGB,
            &[String::from("survey_code")],
        )
        .expect("the Rust core decodes and prepares the golden point records");
    let point_data = &prepared.prepared;

    let source_points = [
        [-123.060_000, 44.052_000, 129.59],
        [-123.060_010, 44.052_005, 120.00],
        [-123.059_990, 44.051_995, 135.59],
        [-123.060_005, 44.052_010, 95.87],
    ];
    let expected_ecef = [
        [
            -2_504_771.189_385_317,
            -3_848_181.046_709_514,
            4_412_337.011_827_192,
        ],
        [
            -2_504_767.890_353_902_7,
            -3_848_174.509_252_852,
            4_412_330.743_093_421,
        ],
        [
            -2_504_773.080_891_284_6,
            -3_848_185.421_731_871,
            4_412_340.784_393_692,
        ],
        [
            -2_504_757.883_204_286,
            -3_848_159.869_378_733,
            4_412_314.364_535_574,
        ],
    ];

    assert_eq!(point_data.point_count, 4);
    assert_eq!(point_data.source_coordinates.len(), 12);
    assert_eq!(point_data.geographic_coordinates.len(), 12);
    assert_eq!(point_data.ecef_coordinates.len(), 12);
    assert_eq!(
        point_data.intensity.as_deref(),
        Some([1_001, 2_002, 3_003, 4_004].as_slice())
    );
    assert_eq!(
        point_data.classification.as_deref(),
        Some([2, 5, 6, 7].as_slice())
    );
    assert_eq!(
        point_data.red.as_deref(),
        Some([1_000, 4_000, 7_000, 10_000].as_slice())
    );
    assert_eq!(
        point_data.green.as_deref(),
        Some([2_000, 5_000, 8_000, 11_000].as_slice())
    );
    assert_eq!(
        point_data.blue.as_deref(),
        Some([3_000, 6_000, 9_000, 12_000].as_slice())
    );

    for (index, source) in source_points.iter().enumerate() {
        let triple = index * 3;
        for (component, expected) in source.iter().enumerate() {
            assert_near(
                point_data.source_coordinates[triple + component],
                *expected,
                1e-12,
                "source XYZ derived from LAS integer coordinate × scale + offset",
            );
        }
        assert_near(
            point_data.geographic_coordinates[triple],
            source[0],
            1e-12,
            "WGS 84 longitude",
        );
        assert_near(
            point_data.geographic_coordinates[triple + 1],
            source[1],
            1e-12,
            "WGS 84 latitude",
        );
        assert_near(
            point_data.geographic_coordinates[triple + 2],
            source[2] + HEIGHT_OFFSET_METERS,
            1e-10,
            "height offset applied once after source conversion",
        );
        for (component, expected) in
            wgs84_ecef(source[0], source[1], source[2] + HEIGHT_OFFSET_METERS)
                .into_iter()
                .enumerate()
        {
            assert_near(
                point_data.ecef_coordinates[triple + component],
                expected_ecef[index][component],
                2e-6,
                "WGS 84 ellipsoid ECEF control",
            );
            assert_near(
                expected,
                expected_ecef[index][component],
                1e-7,
                "independent WGS 84 ECEF formula",
            );
        }
    }

    assert_eq!(prepared.extra_dimensions.len(), 1);
    assert_eq!(prepared.extra_dimensions[0].name, "survey_code");
    assert_eq!(
        prepared.extra_dimensions[0].values,
        DecodedExtraDimensionValues::Float64(vec![65_000.0, 42.0, 17.0, 65_535.0])
    );
    assert_eq!(
        point_data
            .statistics
            .elevation
            .as_ref()
            .map(|range| (range.min, range.max)),
        Some((97.12, 136.84))
    );
    assert_eq!(
        point_data
            .statistics
            .intensity
            .as_ref()
            .map(|range| (range.min, range.max)),
        Some((1_001.0, 4_004.0))
    );
    assert_eq!(point_data.statistics.rgb_max, Some(65_535));
}
