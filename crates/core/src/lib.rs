#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]

mod binary;
mod crs;
mod decoder;
mod error;
mod extra_bytes;
mod header;
mod hierarchy;
mod interleave;
mod metadata;
mod preparation;
mod vlr;

pub use crs::{
    CrsTransform, EcefPoint, GeographicPoint, PreparedCoordinates, geographic_buffer_to_ecef,
    geographic_to_ecef,
};
pub use decoder::{
    CopcNodeDecoder, DEFAULT_MAX_NODE_CHUNK_BYTES, DecodedCopcNode,
    DecodedCopcNodeWithExtraDimensions, DecodedExtraDimension, DecodedExtraDimensionValue,
    DecodedExtraDimensionValues, FIELD_CLASSIFICATION, FIELD_INTENSITY, FIELD_RGB,
    decode_copc_node, decode_copc_node_with_extra_dimensions,
    decode_copc_node_with_max_chunk_bytes,
};
pub use error::{CopcError, Result};
pub use extra_bytes::{ExtraBytesDescriptor, ExtraBytesPrimitive, ExtraBytesValue};
pub use header::{
    CopcHeader, CopcMetadataLayout, CrsWktProvenance, CrsWktSource, EvlrReadPlan, FetchedEvlr,
    ParsedCopcMetadata, copc_metadata_layout, parse_header, parse_header_prefix,
};
pub use hierarchy::{
    RootHierarchy, RootHierarchyNode, RootHierarchyPage, parse_root_hierarchy,
    parse_root_hierarchy_with_max_bytes,
};
pub use interleave::interleave_xyz;
pub use metadata::{parse_copc_metadata, plan_next_evlr_read};
pub use preparation::{
    CopcNodePreparer, PreparedCopcNode, PreparedCopcNodeWithExtraDimensions, PreparedPointRange,
    PreparedPointStatistics,
};
