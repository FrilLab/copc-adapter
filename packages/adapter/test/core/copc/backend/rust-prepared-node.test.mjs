import test from "node:test";
import assert from "node:assert/strict";

import { RustCopcNodePreparer } from "../../../../core/copc/rustCopcNodeDecoder.ts";
import { createCopcPointFieldSelection } from "../../../../core/copc/points/fieldSelection.ts";
import { loadCopcWasm } from "../../../../core/wasm/copcWasm.ts";

const METADATA_HEX =
  "4c41534600000000000000000000000000000000000000000104000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000077016a030000030000008724000000000000000000000000000000000000000000000000007b14ae47e17a843f7b14ae47e17a943fb81e85eb51b89e3f000000000000244000000000000034400000000000003e400000000000005940000000000000f03f000000000000694000000000000000400000000000c072400000000000000840000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000636f70630000000000000000000000000100a000000000000000000000000000000000000000000000000000000000000000000000000000000049400000000000004e400000000000805140000000000000244000000000000004408a030000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004c4153465f50726f6a656374696f6e0040088300000000000000000000000000000000000000000000000000000000000000000047454f4743535b22574753203834222c444154554d5b225747535f31393834222c53504845524f49445b22574753203834222c363337383133372c3239382e3235373232333536335d5d2c5052494d454d5b22477265656e77696368222c305d2c554e49545b22646567726565222c302e303137343533323932353139393433335d5d00006c61737a697020656e636f6465640000bc562e000000000000000000000000000000000000000000000000000000000000000000030000000202000000000000ffffffffffffffffffffffffffffffffffffffff02000a001e0003000b0006000300";
const CHUNK_HEX =
  "64000000c80000002c0100009001000005000000000000000000000000005802bc02200302000000060000000400000005000000000000000400000000000000000000000000000000000000070000000007dd00000004000000060000000008000000aa01aa80800000";
const RESOLVED_WKT =
  'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]';

function fromHex(value) {
  return new Uint8Array(Buffer.from(value, "hex"));
}

function makeEvlrOnlyMetadataPrefix() {
  const metadata = fromHex(METADATA_HEX);
  const wktVlrOffset = 375 + 54 + 160;
  metadata.fill(0, wktVlrOffset + 2, wktVlrOffset + 18);
  metadata.set(new TextEncoder().encode("LASF_Spec"), wktVlrOffset + 2);
  const view = new DataView(metadata.buffer);
  view.setUint16(wktVlrOffset + 18, 7, true);
  view.setFloat64(195, 200, true);
  view.setBigUint64(235, BigInt(metadata.byteLength), true);
  view.setUint32(243, 1, true);
  return metadata;
}

function makeCompleteEvlrOnlyMetadata() {
  const prefix = makeEvlrOnlyMetadataPrefix();
  const payload = new TextEncoder().encode(`${RESOLVED_WKT}\0`);
  const offset = prefix.byteLength;
  const metadata = new Uint8Array(offset + 60 + payload.byteLength);
  metadata.set(prefix);
  metadata.set(new TextEncoder().encode("LASF_Projection"), offset + 2);
  const view = new DataView(metadata.buffer);
  view.setUint16(offset + 18, 2112, true);
  view.setBigUint64(offset + 20, BigInt(payload.byteLength), true);
  metadata.set(payload, offset + 60);
  return metadata;
}

function readParserResponse(wasm, pointer) {
  const memory = new Uint8Array(wasm.memory.buffer);
  let end = pointer;
  while (memory[end] !== 0) end += 1;
  return JSON.parse(new TextDecoder().decode(memory.subarray(pointer, end)));
}

test("shipped Rust WASM executes fused node preparation with host timing", async () => {
  const preparer = await RustCopcNodePreparer.fromMetadata(
    fromHex(METADATA_HEX),
  );
  try {
    const result = preparer.prepare(
      fromHex(CHUNK_HEX),
      2,
      createCopcPointFieldSelection([
        "position",
        "intensity",
        "classification",
        "rgb",
      ]),
    );

    assert.equal(result.prepared.pointCount, 2);
    assert.deepEqual(
      [...result.prepared.source.coordinates],
      [11, 24, 39, 11.01, 24.02, 39.03],
    );
    assert.ok(result.prepared.world.coordinates.every(Number.isFinite));
    assert.deepEqual(result.prepared.statistics, {
      elevation: { min: 39, max: 39.03 },
      intensity: { min: 400, max: 401 },
      rgbMax: 65535,
    });
    assert.deepEqual([...result.prepared.attributes.intensity], [400, 401]);
    assert.deepEqual([...result.prepared.attributes.classification], [5, 6]);
    assert.deepEqual([...result.prepared.attributes.red], [600, 601]);
    assert.ok(result.decodeDurationMs >= 0);
    assert.ok(result.preparationDurationMs >= 0);
  } finally {
    preparer.dispose();
  }
});

test("Rust fused node preparation applies the configured ellipsoidal offset once", async () => {
  const source = fromHex(METADATA_HEX);
  const preparer = await RustCopcNodePreparer.fromMetadata(
    source,
    undefined,
    undefined,
    12.5,
  );
  try {
    const result = preparer.prepare(
      fromHex(CHUNK_HEX),
      2,
      createCopcPointFieldSelection(["position"]),
    );
    assert.deepEqual(
      [...result.prepared.source.coordinates],
      [11, 24, 39, 11.01, 24.02, 39.03],
    );
    const geographic = result.prepared.geographic.coordinates;
    assert.ok(Math.abs(geographic[0] - 11) < 1e-12);
    assert.ok(Math.abs(geographic[1] - 24) < 1e-12);
    assert.equal(geographic[2], 51.5);
    assert.ok(Math.abs(geographic[3] - 11.01) < 1e-12);
    assert.ok(Math.abs(geographic[4] - 24.02) < 1e-12);
    assert.equal(geographic[5], 51.53);
    assert.deepEqual(result.prepared.statistics.elevation, {
      min: 51.5,
      max: 51.53,
    });
    const repeated = preparer.prepare(
      fromHex(CHUNK_HEX),
      2,
      createCopcPointFieldSelection(["position"]),
    );
    assert.deepEqual(
      [...repeated.prepared.source.coordinates],
      [...result.prepared.source.coordinates],
    );
    assert.deepEqual(
      [...repeated.prepared.geographic.coordinates],
      [...geographic],
    );
  } finally {
    preparer.dispose();
  }
});

test("Rust fused preparation accepts WKT resolved from an EVLR outside the prefix", async () => {
  const metadata = makeEvlrOnlyMetadataPrefix();
  await assert.rejects(
    () => RustCopcNodePreparer.fromMetadata(metadata),
    (error) => error?.code === "missing-wkt",
  );

  const preparer = await RustCopcNodePreparer.fromMetadata(
    metadata,
    undefined,
    undefined,
    0,
    RESOLVED_WKT,
  );
  try {
    const result = preparer.prepare(
      fromHex(CHUNK_HEX),
      2,
      createCopcPointFieldSelection(["position"]),
    );
    assert.deepEqual(
      [...result.prepared.source.coordinates],
      [11, 24, 39, 11.01, 24.02, 39.03],
    );
    assert.ok(result.prepared.world.coordinates.every(Number.isFinite));
  } finally {
    preparer.dispose();
  }
});

test("legacy Rust WASM preparer exports resolve WKT from complete EVLR metadata", async () => {
  const wasm = await loadCopcWasm();
  const metadata = makeCompleteEvlrOnlyMetadata();
  const metadataPointer = wasm.alloc_bytes(metadata.byteLength);
  new Uint8Array(
    wasm.memory.buffer,
    metadataPointer,
    metadata.byteLength,
  ).set(metadata);

  const createPreparers = [
    () => wasm.create_copc_node_preparer_json(metadataPointer, metadata.byteLength),
    () =>
      wasm.create_copc_node_preparer_with_limit_json(
        metadataPointer,
        metadata.byteLength,
        64 * 1024 * 1024,
      ),
    () =>
      wasm.create_copc_node_preparer_with_limit_and_height_offset_json(
        metadataPointer,
        metadata.byteLength,
        64 * 1024 * 1024,
        0,
      ),
  ];

  try {
    for (const createPreparer of createPreparers) {
      const responsePointer = createPreparer();
      try {
        const response = readParserResponse(wasm, responsePointer);
        assert.equal(response.ok, true, response.error?.message);
        assert.equal(response.value.used_horizontal_fallback, false);
        wasm.free_copc_node_preparer(response.value.handle);
      } finally {
        wasm.free_parser_json(responsePointer);
      }
    }
  } finally {
    wasm.dealloc_bytes(metadataPointer, metadata.byteLength);
  }
});
