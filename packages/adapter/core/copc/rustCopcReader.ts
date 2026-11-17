import { loadCopcWasm } from "../wasm/copcWasm";
import {
  decodeRustCopcNode,
  getRustPointFieldMask,
  RustCopcNodePreparer,
} from "./rustCopcNodeDecoder";
import { RustCopcDecodeWorkerPool } from "./rustCopcDecodeWorkerPool";
import type {
  CopcHierarchyNode,
  CopcHierarchyPage,
  CopcHierarchySubtree,
} from "./hierarchy/types";
import type {
  CopcMetadata,
  CopcPointBuffer,
  PreparedPointData,
} from "./types/copc";
import type { RandomAccessByteSource } from "./range/types";
import type { CopcPointLoadOptions } from "./backend/types";
import type { CopcExtraDimensionSelection } from "./points/extraDimensions";
import {
  mapRustCopcExtraDimensionMetadata,
  normalizeCopcExtraDimensionSelection,
  resolveCopcExtraDimensionRequest,
} from "./points/extraDimensions";
import type { CopcHierarchyPageLoadOptions } from "./hierarchy/types";
import type { CopcPointFieldSelection } from "./points/fieldSelection";
import type { CopcWorkerDiagnostics } from "./backend/types";
import { performanceNow, type CopcPerformanceObserver } from "./performance";
import { resolveMaxRangeBytes, resolveWasmMaxRangeBytes } from "./range/types";
import { resolveHeightOffsetMeters } from "../coordinates/transform/heightOffset";
import { resolveCopcCrsMetadata } from "../coordinates/crs/copcCrsMetadata";
import type { CopcCrsWktProvenance } from "../coordinates/crs/copcCrsMetadata";

const INITIAL_LAS_HEADER_LENGTH = 375;
const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;
const MAX_DECODE_POINTS = 5_000_000;

export type RustCopcReaderOptions = {
  /** Maximum source range length in bytes; defaults to the shared 64 MiB limit. */
  readonly maxRangeBytes?: number;
  /** Additive correction to WGS84 ellipsoidal point heights, in metres; defaults to 0. */
  readonly heightOffsetMeters?: number;
  /** Extra Bytes dimensions to retain for each point; omitted keeps them opt-in. */
  readonly extraDimensions?: CopcExtraDimensionSelection;
};

type RustHeaderValue = {
  /** LAS file-format major version. */
  version_major: number;
  /** LAS file-format minor version. */
  version_minor: number;
  /** LAS point data record format identifier. */
  point_data_record_format: number;
  /** Point record length in bytes. */
  point_data_record_length: number;
  /** Total source point count. */
  point_count: number;
  /** LAS X, Y, and Z scale factors. */
  scale: [number, number, number];
  /** LAS X, Y, and Z offsets. */
  offset: [number, number, number];
  /** Minimum and maximum source bounds in XYZ order. */
  bounds: [number, number, number, number, number, number];
  /** COPC root cube bounds in source-coordinate units. */
  cube: [number, number, number, number, number, number];
  /** Nominal point spacing in source-coordinate units. */
  spacing: number;
  /** Root hierarchy page offset in bytes. */
  root_hierarchy_page_offset: number;
  /** Root hierarchy page length in bytes. */
  root_hierarchy_page_length: number;
  /** Source CRS WKT, when present. */
  wkt?: string;
  /** Rust-resolved WKT source and cross-record conflict status. */
  crs_wkt_provenance: CopcCrsWktProvenance;
  extra_dimensions: Array<{
    name: string;
    type?: "signed" | "unsigned" | "float" | null;
    length: number;
    byte_offset: number;
    scale?: number | null;
    offset?: number | null;
    no_data?: number | null;
    no_data_exact?: string | null;
    supported_scalar?: boolean | null;
  }>;
};

type RustHierarchyValue = {
  entry_count: number;
  nodes: Array<{
    level: number;
    x: number;
    y: number;
    z: number;
    point_data_offset: number;
    point_data_length: number;
    point_count: number;
  }>;
  pages: Array<{
    level: number;
    x: number;
    y: number;
    z: number;
    page_offset: number;
    page_length: number;
  }>;
};

type RustParserResponse<T> = {
  ok: boolean;
  value?: T;
  error?: {
    code: string;
    message: string;
  };
};

export { RustCopcParseError } from "./rustCopcErrors";
export type { RustCopcParseErrorCode } from "./rustCopcErrors";
import { requireRustWasmPointer, RustCopcParseError } from "./rustCopcErrors";

/** Header fields parsed from the COPC source by the Rust/WASM reader. */
export type RustCopcHeader = RustHeaderValue;

function requireDecodePointCount(value: number): number {
  const count = requireSafeInteger(value, "point count");
  if (count === 0 || count > MAX_DECODE_POINTS) {
    throw new RustCopcParseError(
      "unsupported-value",
      `point count must be between 1 and ${MAX_DECODE_POINTS}`,
    );
  }
  return count;
}

function requireSafeInteger(value: number, name: string): number {
  if (!Number.isSafeInteger(value) || value < 0 || value > MAX_SAFE_INTEGER) {
    throw new RustCopcParseError(
      "unsupported-value",
      `${name} is not a non-negative JavaScript-safe integer`,
    );
  }
  return value;
}

function requirePositiveSafeInteger(value: number, name: string): number {
  const checked = requireSafeInteger(value, name);
  if (checked === 0) {
    throw new RustCopcParseError("invalid-value", `${name} must be positive`);
  }
  return checked;
}

function requireHierarchyPageLength(
  value: number,
  maxRangeBytes: number,
): number {
  const checked = requirePositiveSafeInteger(value, "hierarchy page length");
  if (checked > maxRangeBytes) {
    throw new RustCopcParseError(
      "unsupported-value",
      `hierarchy page exceeds the ${maxRangeBytes}-byte safety limit`,
    );
  }
  return checked;
}

function readCString(memory: WebAssembly.Memory, pointer: number): string {
  const bytes = new Uint8Array(memory.buffer);
  if (pointer === 0) {
    throw new RustCopcParseError(
      "serialization",
      "Rust parser could not allocate a response",
    );
  }
  if (
    !Number.isSafeInteger(pointer) ||
    pointer < 0 ||
    pointer >= bytes.length
  ) {
    throw new RustCopcParseError(
      "invalid-input",
      "Rust parser returned an invalid response pointer",
    );
  }
  let end = pointer;
  while (end < bytes.length && bytes[end] !== 0) {
    end += 1;
  }
  if (end === bytes.length) {
    throw new RustCopcParseError(
      "invalid-input",
      "Rust parser returned an unterminated response",
    );
  }
  return new TextDecoder().decode(bytes.subarray(pointer, end));
}

async function parseWithRust<T>(
  bytes: Uint8Array,
  parse: (
    wasm: Awaited<ReturnType<typeof loadCopcWasm>>,
    pointer: number,
    length: number,
  ) => number,
  wasmPromise?: ReturnType<typeof loadCopcWasm>,
): Promise<T> {
  const wasm = await (wasmPromise ?? loadCopcWasm());
  const inputPointer = requireRustWasmPointer(
    wasm.alloc_bytes(bytes.byteLength),
    bytes.byteLength,
    "parser input",
  );
  new Uint8Array(wasm.memory.buffer, inputPointer, bytes.byteLength).set(bytes);

  try {
    const responsePointer = parse(wasm, inputPointer, bytes.byteLength);
    try {
      const response = JSON.parse(
        readCString(wasm.memory, responsePointer),
      ) as RustParserResponse<T>;
      if (!response.ok || response.value === undefined) {
        throw new RustCopcParseError(
          response.error?.code ?? "invalid-input",
          response.error?.message ??
            "Rust parser returned an invalid error response",
        );
      }
      return response.value;
    } finally {
      wasm.free_parser_json(responsePointer);
    }
  } finally {
    wasm.dealloc_bytes(inputPointer, bytes.byteLength);
  }
}

type FetchedEvlrRecord = {
  readonly offset: number;
  readonly header: Uint8Array;
  readonly payload?: Uint8Array;
};

type RustMetadataLayout = {
  header_size: number;
  point_data_offset: number;
  first_evlr_offset: number;
  evlr_count: number;
};

type RustEvlrReadPlan = {
  next_header_offset?: number;
  payload_offset: number;
  wkt_payload_length?: number;
};

const EVLR_HEADER_BYTES = 60;
const EVLR_FRAME_HEADER_BYTES = 76;
const UNKNOWN_SOURCE_SIZE = BigInt("18446744073709551615");

function metadataPrefixRangeHint(
  initialBytes: Uint8Array,
  maxRangeBytes: number,
): number | undefined {
  if (initialBytes.byteLength < 100) return undefined;
  const view = new DataView(
    initialBytes.buffer,
    initialBytes.byteOffset,
    initialBytes.byteLength,
  );
  const pointDataOffset = view.getUint32(96, true);
  return pointDataOffset > initialBytes.byteLength && pointDataOffset <= maxRangeBytes
    ? pointDataOffset
    : undefined;
}

async function parseWithRustInputs<T>(
  inputs: readonly Uint8Array[],
  parse: (
    wasm: Awaited<ReturnType<typeof loadCopcWasm>>,
    pointers: readonly number[],
    lengths: readonly number[],
  ) => number,
  wasmPromise?: ReturnType<typeof loadCopcWasm>,
): Promise<T> {
  const wasm = await (wasmPromise ?? loadCopcWasm());
  const allocations: Array<{ pointer: number; length: number }> = [];
  try {
    for (const bytes of inputs) {
      const pointer = requireRustWasmPointer(
        wasm.alloc_bytes(bytes.byteLength),
        bytes.byteLength,
        "parser input",
      );
      allocations.push({ pointer, length: bytes.byteLength });
      new Uint8Array(wasm.memory.buffer, pointer, bytes.byteLength).set(bytes);
    }
    const responsePointer = parse(
      wasm,
      allocations.map(({ pointer }) => pointer),
      allocations.map(({ length }) => length),
    );
    try {
      const response = JSON.parse(
        readCString(wasm.memory, responsePointer),
      ) as RustParserResponse<T>;
      if (!response.ok || response.value === undefined) {
        throw new RustCopcParseError(
          response.error?.code ?? "invalid-input",
          response.error?.message ?? "Rust parser returned an invalid error response",
        );
      }
      return response.value;
    } finally {
      wasm.free_parser_json(responsePointer);
    }
  } finally {
    for (const { pointer, length } of allocations) {
      wasm.dealloc_bytes(pointer, length);
    }
  }
}

function encodeEvlrDirectory(records: readonly FetchedEvlrRecord[]): Uint8Array {
  const length = records.reduce(
    (total, record) => total + EVLR_FRAME_HEADER_BYTES + (record.payload?.byteLength ?? 0),
    0,
  );
  if (!Number.isSafeInteger(length)) {
    throw new RustCopcParseError("overflow", "EVLR metadata directory is too large");
  }
  const result = new Uint8Array(length);
  const view = new DataView(result.buffer);
  let offset = 0;
  for (const record of records) {
    view.setBigUint64(offset, BigInt(record.offset), true);
    result.set(record.header, offset + 8);
    view.setBigUint64(
      offset + 8 + EVLR_HEADER_BYTES,
      record.payload ? BigInt(record.payload.byteLength) : UNKNOWN_SOURCE_SIZE,
      true,
    );
    offset += EVLR_FRAME_HEADER_BYTES;
    if (record.payload) {
      result.set(record.payload, offset);
      offset += record.payload.byteLength;
    }
  }
  return result;
}

function key(level: number, x: number, y: number, z: number): string {
  return `${level}-${x}-${y}-${z}`;
}

function toHierarchySubtree(
  value: RustHierarchyValue,
  maxRangeBytes: number,
): CopcHierarchySubtree {
  return {
    nodes: value.nodes.map(
      (node): CopcHierarchyNode => ({
        key: key(node.level, node.x, node.y, node.z),
        level: node.level,
        x: node.x,
        y: node.y,
        z: node.z,
        pointCount: requireSafeInteger(
          node.point_count,
          "hierarchy point count",
        ),
        pointDataOffset: requireSafeInteger(
          node.point_data_offset,
          "point data offset",
        ),
        pointDataLength: requireSafeInteger(
          node.point_data_length,
          "point data length",
        ),
      }),
    ),
    pages: value.pages.map(
      (page): CopcHierarchyPage => ({
        key: key(page.level, page.x, page.y, page.z),
        pageOffset: requireSafeInteger(
          page.page_offset,
          "hierarchy page offset",
        ),
        pageLength: requireHierarchyPageLength(page.page_length, maxRangeBytes),
      }),
    ),
  };
}

/**
 * Header/root reader backed by the project-owned random-access source.
 * TypeScript owns I/O; Rust only validates and interprets COPC bytes.
 */
export class RustCopcReader {
  /** Resource identifier of the supplied byte source. */
  readonly source: string;
  /** Parsed LAS/COPC header values. */
  readonly header: RustCopcHeader;
  /** Per-range read ceiling in bytes used by the reader and worker pool. */
  readonly maxRangeBytes: number;
  /** Additive ellipsoidal-height correction used by prepared-point reads. */
  readonly heightOffsetMeters: number;
  /** Source-wide Extra Bytes selection after backend and open options merge. */
  readonly configuredExtraDimensions?: CopcExtraDimensionSelection;
  private readonly byteSource: RandomAccessByteSource;
  private readonly crsWktProvenance: CopcCrsWktProvenance;
  private readonly decodeWorkerPool?: RustCopcDecodeWorkerPool;
  private nodePreparer?: Promise<RustCopcNodePreparer>;
  private performanceObserver?: CopcPerformanceObserver;

  private constructor(
    byteSource: RandomAccessByteSource,
    header: RustCopcHeader,
    maxRangeBytes: number,
    heightOffsetMeters: number,
    configuredExtraDimensions?: CopcExtraDimensionSelection,
  ) {
    this.byteSource = byteSource;
    this.source = byteSource.source;
    this.header = header;
    this.maxRangeBytes = maxRangeBytes;
    this.heightOffsetMeters = heightOffsetMeters;
    this.configuredExtraDimensions = configuredExtraDimensions;
    this.crsWktProvenance = header.crs_wkt_provenance;
    this.decodeWorkerPool =
      typeof Worker !== "undefined"
        ? new RustCopcDecodeWorkerPool({ maxRangeBytes, heightOffsetMeters })
        : undefined;
  }

  private metadataBytes: Uint8Array | undefined;

  /** Replace or clear the observer for completed range, decode, and preparation measurements. */
  setPerformanceObserver(observer: CopcPerformanceObserver | undefined): void {
    this.performanceObserver = observer;
  }

  private async readRange(
    offset: number,
    length: number,
    nodeKey?: string,
    signal?: AbortSignal,
    performanceObserver?: CopcPerformanceObserver | null,
  ): Promise<Uint8Array> {
    const startedAt = performanceNow();
    const bytes = await this.byteSource.readRange(offset, length, { signal });
    this.resolvePerformanceObserver(performanceObserver)?.({
      stage: "rangeFetch",
      durationMs: performanceNow() - startedAt,
      nodeKey,
      bytes: bytes.byteLength,
    });
    return bytes;
  }

  private resolvePerformanceObserver(
    override?: CopcPerformanceObserver | null,
  ): CopcPerformanceObserver | undefined {
    return override === undefined
      ? this.performanceObserver
      : (override ?? undefined);
  }

  /**
   * Read and validate source metadata, then create a reader over the supplied byte source.
   *
   * The reader owns its decode workers and preparer, but does not destroy the
   * caller-supplied byte source.
   *
   * @throws Parser or byte-source errors when the header or metadata cannot be read or validated.
   */
  static async open(
    byteSource: RandomAccessByteSource,
    options: RustCopcReaderOptions = {},
  ): Promise<RustCopcReader> {
    const maxRangeBytes = resolveMaxRangeBytes(
      byteSource.source,
      options.maxRangeBytes,
    );
    const heightOffsetMeters = resolveHeightOffsetMeters(
      options.heightOffsetMeters,
    );
    // Load WASM alongside the initial source read. A bounded prefix read can
    // then overlap Rust layout validation below.
    const wasmPromise = loadCopcWasm();
    void wasmPromise.catch(() => undefined);
    const initialBytes = await byteSource.readRange(0, INITIAL_LAS_HEADER_LENGTH);
    // The LAS point-data offset is only an I/O scheduling hint here. Rust
    // validates the header and the hint is used only if Rust returns the same
    // bounded prefix length.
    const prefixRangeHint = metadataPrefixRangeHint(initialBytes, maxRangeBytes);
    const prefetchedPrefix =
      prefixRangeHint === undefined
        ? undefined
        : byteSource.readRange(0, prefixRangeHint).then(
            (bytes) => ({ bytes }),
            (error: unknown) => ({ error }),
          );
    const layout = await parseWithRust<RustMetadataLayout>(
      initialBytes,
      (wasm, pointer, length) =>
        wasm.plan_copc_metadata_layout_json(
          pointer,
          length,
          resolveWasmMaxRangeBytes(byteSource.source, maxRangeBytes),
        ),
      wasmPromise,
    );
    const pointDataOffset = requireSafeInteger(
      layout.point_data_offset,
      "point data offset",
    );
    let metadataPrefix: Uint8Array;
    if (pointDataOffset <= initialBytes.byteLength) {
      metadataPrefix = initialBytes.slice(0, pointDataOffset);
    } else if (prefixRangeHint === pointDataOffset && prefetchedPrefix) {
      const prefetched = await prefetchedPrefix;
      if ("error" in prefetched) throw prefetched.error;
      metadataPrefix = prefetched.bytes;
    } else {
      metadataPrefix = await byteSource.readRange(0, pointDataOffset);
    }
    if (metadataPrefix.byteLength !== pointDataOffset) {
      throw new RustCopcParseError(
        "truncated",
        `LAS metadata prefix requires ${pointDataOffset} bytes, received ${metadataPrefix.byteLength}`,
      );
    }

    let sourceSize: number | undefined;
    if (layout.evlr_count > 0) {
      const size = await byteSource.size();
      if (size !== undefined) sourceSize = requireSafeInteger(size, "source size");
    }
    const sourceSizeArgument =
      sourceSize === undefined ? UNKNOWN_SOURCE_SIZE : BigInt(sourceSize);
    let expectedEvlrOffset = requireSafeInteger(
      layout.first_evlr_offset,
      "first EVLR offset",
    );
    let fetchedWktBytes = 0;
    const evlrs: FetchedEvlrRecord[] = [];
    for (let index = 0; index < layout.evlr_count; index += 1) {
      const evlrHeaderEnd = expectedEvlrOffset + EVLR_HEADER_BYTES;
      if (!Number.isSafeInteger(evlrHeaderEnd)) {
        throw new RustCopcParseError("overflow", "EVLR header range exceeds JavaScript-safe integers");
      }
      if (sourceSize !== undefined && evlrHeaderEnd > sourceSize) {
        throw new RustCopcParseError("invalid-header", "LAS EVLR header exceeds the source size");
      }
      const evlrHeader = await byteSource.readRange(
        expectedEvlrOffset,
        EVLR_HEADER_BYTES,
      );
      const plan = await parseWithRustInputs<RustEvlrReadPlan>(
        [initialBytes, evlrHeader],
        (wasm, pointers, lengths) =>
          wasm.plan_next_copc_evlr_read_json(
            pointers[0]!,
            lengths[0]!,
            index,
            BigInt(expectedEvlrOffset),
            pointers[1]!,
            lengths[1]!,
            sourceSizeArgument,
            BigInt(fetchedWktBytes),
            resolveWasmMaxRangeBytes(byteSource.source, maxRangeBytes),
          ),
        wasmPromise,
      );
      let payload: Uint8Array | undefined;
      if (plan.wkt_payload_length !== undefined && plan.wkt_payload_length !== null) {
        const payloadLength = requireSafeInteger(
          plan.wkt_payload_length,
          "WKT EVLR payload length",
        );
        const payloadOffset = requireSafeInteger(
          plan.payload_offset,
          "WKT EVLR payload offset",
        );
        payload =
          payloadLength === 0
            ? new Uint8Array()
            : await byteSource.readRange(payloadOffset, payloadLength);
        if (payload.byteLength !== payloadLength) {
          throw new RustCopcParseError(
            "truncated",
            "WKT EVLR payload read did not return the declared byte length",
          );
        }
        fetchedWktBytes += payloadLength;
      }
      evlrs.push({ offset: expectedEvlrOffset, header: evlrHeader, payload });
      if (plan.next_header_offset !== undefined && plan.next_header_offset !== null) {
        expectedEvlrOffset = requireSafeInteger(
          plan.next_header_offset,
          "next EVLR header offset",
        );
      }
    }
    const directory = encodeEvlrDirectory(evlrs);
    const parsedHeader = await parseWithRustInputs<RustCopcHeader>(
      [metadataPrefix, directory],
      (wasm, pointers, lengths) =>
        wasm.parse_copc_metadata_json(
          pointers[0]!,
          lengths[0]!,
          pointers[1]!,
          lengths[1]!,
          sourceSizeArgument,
          resolveWasmMaxRangeBytes(byteSource.source, maxRangeBytes),
        ),
      wasmPromise,
    );
    const header: RustCopcHeader = {
      ...parsedHeader,
      wkt: parsedHeader.wkt ?? undefined,
      extra_dimensions: parsedHeader.extra_dimensions ?? [],
    };
    const reader = new RustCopcReader(
      byteSource,
      header,
      maxRangeBytes,
      heightOffsetMeters,
      normalizeCopcExtraDimensionSelection(options.extraDimensions),
    );
    reader.metadataBytes = metadataPrefix;
    reader.decodeWorkerPool?.setMetadata(metadataPrefix, header.wkt);
    return reader;
  }

  /** Return normalized COPC metadata, preserving source CRS units and WKT. */
  getMetadata(): CopcMetadata {
    const { bounds, cube, scale, offset } = this.header;
    return {
      pointCount: requireSafeInteger(this.header.point_count, "point count"),
      bounds: {
        minX: bounds[0],
        minY: bounds[1],
        minZ: bounds[2],
        maxX: bounds[3],
        maxY: bounds[4],
        maxZ: bounds[5],
      },
      spacing: this.header.spacing,
      scale: { x: scale[0], y: scale[1], z: scale[2] },
      offset: { x: offset[0], y: offset[1], z: offset[2] },
      cube: {
        minX: cube[0],
        minY: cube[1],
        minZ: cube[2],
        maxX: cube[3],
        maxY: cube[4],
        maxZ: cube[5],
      },
      wkt: this.header.wkt,
      crsWktProvenance: {
        ...this.crsWktProvenance,
        available: [...this.crsWktProvenance.available],
      },
      crsDiagnostic: resolveCopcCrsMetadata(this.header.wkt, this.crsWktProvenance),
      extraDimensions: mapRustCopcExtraDimensionMetadata(this.header.extra_dimensions),
    };
  }

  /** Return the root hierarchy byte range parsed from the COPC header. */
  getRootHierarchyPage(): CopcHierarchyPage {
    return {
      key: "0-0-0-0",
      pageOffset: requireSafeInteger(
        this.header.root_hierarchy_page_offset,
        "root hierarchy page offset",
      ),
      pageLength: requireHierarchyPageLength(
        this.header.root_hierarchy_page_length,
        this.maxRangeBytes,
      ),
    };
  }

  /** Fetch and parse one hierarchy page through the supplied random-access source. */
  async loadHierarchyPage(
    page: CopcHierarchyPage,
    options: CopcHierarchyPageLoadOptions = {},
  ): Promise<CopcHierarchySubtree> {
    const pageOffset = requireSafeInteger(
      page.pageOffset,
      "hierarchy page offset",
    );
    const pageLength = requireHierarchyPageLength(
      page.pageLength,
      this.maxRangeBytes,
    );
    const end = pageOffset + pageLength;
    if (!Number.isSafeInteger(end)) {
      throw new RustCopcParseError(
        "overflow",
        "hierarchy page range exceeds JavaScript-safe integers",
      );
    }
    const bytes = await this.readRange(
      pageOffset,
      pageLength,
      undefined,
      options.signal,
      options.performanceObserver,
    );
    throwIfRangeAborted(options.signal);
    const parsed = await parseWithRust<RustHierarchyValue>(
      bytes,
      (wasm, pointer, length) =>
        wasm.parse_root_hierarchy_with_limit_json(
          pointer,
          length,
          resolveWasmMaxRangeBytes(this.source, this.maxRangeBytes),
        ),
      );
    throwIfRangeAborted(options.signal);
    return toHierarchySubtree(parsed, this.maxRangeBytes);
  }

  /** Load the root page only; referenced child pages remain lazy. */
  async loadRootHierarchy(): Promise<CopcHierarchySubtree> {
    return this.loadHierarchyPage(this.getRootHierarchyPage());
  }

  /**
   * Read and decode one hierarchy node's compressed LAZ chunk.
   *
   * TypeScript performs the byte-range read and Rust decodes the requested
   * fields. The returned coordinates remain in COPC source units.
   *
   * @throws If the node range, point count, requested fields, or LAZ data is invalid; abort rejects this operation.
   */
  async loadPointDataBuffer(
    node: CopcHierarchyNode,
    fields: CopcPointFieldSelection,
    options: CopcPointLoadOptions = {},
  ): Promise<CopcPointBuffer> {
    const extraDimensions = this.resolveExtraDimensions(options.extraDimensions);
    const { metadataBytes, chunkBytes, pointCount } = await this.readNodeChunk(
      node,
      fields,
      options.signal,
      options.performanceObserver,
    );
    throwIfPointLoadAborted(options.signal);
    if (this.decodeWorkerPool) {
      const decoded = await this.decodeWorkerPool.submit(
        {
          nodeKey: node.key,
          pointCount,
          requestedFields: getRustPointFieldMask(fields),
          extraDimensions,
          chunk: chunkBytes,
        },
        options.workload ?? "view",
        options.signal,
      );
      this.resolvePerformanceObserver(options.performanceObserver)?.({
        stage: "decode",
        durationMs: decoded.decodeDurationMs,
        nodeKey: node.key,
        blocksMainThread: false,
      });
      const { decodeDurationMs: _decodeDurationMs, ...buffer } = decoded;
      throwIfPointLoadAborted(options.signal);
      return buffer;
    }

    const decoded = await decodeRustCopcNode(
      metadataBytes,
      chunkBytes,
      pointCount,
      fields,
      loadCopcWasm,
      this.maxRangeBytes,
      extraDimensions,
    );
    this.resolvePerformanceObserver(options.performanceObserver)?.({
      stage: "decode",
      durationMs: decoded.durationMs,
      nodeKey: node.key,
      blocksMainThread: true,
    });
    throwIfPointLoadAborted(options.signal);
    return decoded.buffer;
  }

  /**
   * Decode one node and prepare source, WGS84 geographic, and ECEF buffers.
   *
   * Height correction is applied in metres to WGS84 ellipsoidal height. When
   * Workers are available, decode and preparation run off the main thread.
   *
   * @throws If reading, decoding, CRS preparation, or cancellation fails.
   */
  async loadPreparedPointData(
    node: CopcHierarchyNode,
    fields: CopcPointFieldSelection,
    options: CopcPointLoadOptions = {},
  ): Promise<PreparedPointData> {
    const extraDimensions = this.resolveExtraDimensions(options.extraDimensions);
    const { metadataBytes, chunkBytes, pointCount } = await this.readNodeChunk(
      node,
      fields,
      options.signal,
      options.performanceObserver,
    );
    throwIfPointLoadAborted(options.signal);
    const request = {
      nodeKey: node.key,
      pointCount,
      requestedFields: getRustPointFieldMask(fields),
      extraDimensions,
      chunk: chunkBytes,
    };
    if (this.decodeWorkerPool) {
      const result = await this.decodeWorkerPool.submitPrepared(
        request,
        options.workload ?? "view",
        options.signal,
      );
      this.resolvePerformanceObserver(options.performanceObserver)?.({
        stage: "decode",
        durationMs: result.decodeDurationMs,
        nodeKey: node.key,
        blocksMainThread: false,
      });
      this.resolvePerformanceObserver(options.performanceObserver)?.({
        stage: "pointPreparation",
        durationMs: result.preparationDurationMs,
        nodeKey: node.key,
        blocksMainThread: false,
      });
      throwIfPointLoadAborted(options.signal);
      return result.prepared;
    }

    if (!this.nodePreparer) {
      this.nodePreparer = RustCopcNodePreparer.fromMetadata(
        metadataBytes,
        loadCopcWasm,
        this.maxRangeBytes,
        this.heightOffsetMeters,
        this.header.wkt,
      );
    }
    const result = (await this.nodePreparer).prepare(
      chunkBytes,
      pointCount,
      fields,
      extraDimensions,
    );
    this.resolvePerformanceObserver(options.performanceObserver)?.({
      stage: "decode",
      durationMs: result.decodeDurationMs,
      nodeKey: node.key,
      blocksMainThread: true,
    });
    this.resolvePerformanceObserver(options.performanceObserver)?.({
      stage: "pointPreparation",
      durationMs: result.preparationDurationMs,
      nodeKey: node.key,
      blocksMainThread: true,
    });
    throwIfPointLoadAborted(options.signal);
    return result.prepared;
  }

  private resolveExtraDimensions(
    requested: readonly string[] | undefined,
  ): readonly string[] {
    return resolveCopcExtraDimensionRequest(
      this.configuredExtraDimensions,
      requested,
      this.header.extra_dimensions.map(({ name }) => name),
    ) ?? [];
  }

  private async readNodeChunk(
    node: CopcHierarchyNode,
    fields: CopcPointFieldSelection,
    signal?: AbortSignal,
    performanceObserver?: CopcPerformanceObserver | null,
  ): Promise<{
    metadataBytes: Uint8Array;
    chunkBytes: Uint8Array;
    pointCount: number;
  }> {
    if (!fields.has("position")) {
      throw new RustCopcParseError(
        "invalid-value",
        "COPC point selection must include position",
      );
    }
    const pointCount = requireDecodePointCount(node.pointCount);
    const coordinateLength = pointCount * 3;
    if (!Number.isSafeInteger(coordinateLength)) {
      throw new RustCopcParseError(
        "overflow",
        "coordinate output length exceeds safe integer range",
      );
    }
    if (
      !Number.isSafeInteger(node.pointDataLength) ||
      node.pointDataLength > this.maxRangeBytes
    ) {
      throw new RustCopcParseError(
        "unsupported-value",
        `point data chunk must not exceed ${this.maxRangeBytes} bytes`,
      );
    }

    const metadataBytes = this.metadataBytes;
    if (!metadataBytes) {
      throw new RustCopcParseError(
        "invalid-input",
        "Rust COPC reader metadata is unavailable",
      );
    }
    const chunkBytes = await this.readRange(
      node.pointDataOffset,
      node.pointDataLength,
      node.key,
      signal,
      performanceObserver,
    );
    return { metadataBytes, chunkBytes, pointCount };
  }

  /** Cancel queued render-view jobs while preserving independent spatial queries. */
  cancelPendingPointJobs(): void {
    this.decodeWorkerPool?.cancelQueued(
      (_request, workload) => workload === "view",
    );
  }

  /** Return worker queue counters, or `undefined` when no worker pool is available. */
  getWorkerDiagnostics(): CopcWorkerDiagnostics | undefined {
    return this.decodeWorkerPool?.getDiagnostics();
  }

  /** Release reader-owned worker and CRS preparation resources, not the supplied byte source. */
  destroy(): void {
    void this.nodePreparer?.then(
      (preparer) => preparer.dispose(),
      () => undefined,
    );
    this.decodeWorkerPool?.destroy();
  }
}

function throwIfRangeAborted(signal?: AbortSignal): void {
  if (!signal?.aborted) return;
  const error = new Error("COPC hierarchy page load was cancelled", {
    cause: signal.reason,
  });
  error.name = "AbortError";
  throw error;
}

function throwIfPointLoadAborted(signal?: AbortSignal): void {
  if (!signal?.aborted) {
    return;
  }
  const error = new Error("Point-node load was cancelled", {
    cause: signal.reason,
  });
  error.name = "AbortError";
  throw error;
}
