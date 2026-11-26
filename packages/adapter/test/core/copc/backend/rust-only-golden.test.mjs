import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { File } from "node:buffer";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Keep this import graph independent from the package root and CopcJsBackend.
import { RustCopcBackend } from "../../../../core/copc/backend/rustCopcBackend.ts";
import { CopcBackendError } from "../../../../core/copc/errors.ts";
import { HttpRangeByteSource } from "../../../../core/copc/range/httpRangeSource.ts";
import { InMemoryByteSource } from "../../../../core/copc/range/inMemoryByteSource.ts";

const fixturePath = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../../../crates/core/tests/fixtures/rust-only-golden-pf8.copc.laz",
);
const HEIGHT_OFFSET_METERS = 1.25;

function assertArrayNear(actual, expected, tolerance, label) {
  assert.equal(actual.length, expected.length, label);
  for (let index = 0; index < expected.length; index += 1) {
    assert.ok(
      Math.abs(actual[index] - expected[index]) <= tolerance,
      `${label}[${index}]: got ${actual[index]}, expected ${expected[index]} ± ${tolerance}`,
    );
  }
}

async function openGoldenSource(input, label, options = {}) {
  const backend = new RustCopcBackend({
    ...options,
    extraDimensions: ["survey_code"],
  });
  return backend.open(input, { heightOffsetMeters: HEIGHT_OFFSET_METERS });
}

test("Rust/WASM directly reads independent golden data through custom, Blob, and File sources", async () => {
  const fixture = await readFile(fixturePath);
  const fileBytes = new Uint8Array(
    fixture.buffer,
    fixture.byteOffset,
    fixture.byteLength,
  );
  const cases = [
    {
      label: "custom random access source",
      input: "memory://rust-only-golden.copc.laz",
      options: {
        createByteSource: (source) => new InMemoryByteSource(fileBytes, source),
      },
    },
    {
      label: "Blob",
      input: new Blob([fileBytes], { type: "application/vnd.laszip" }),
    },
    {
      label: "File",
      input: new File([fileBytes], "rust-only-golden-pf8.copc.laz"),
    },
  ];

  for (const { label, input, options } of cases) {
    const source = await openGoldenSource(input, label, options);
    try {
      const metadata = source.getMetadata();
      assert.equal(metadata.pointCount, 4, label);
      assert.equal(metadata.scale.x, 0.000001, label);
      assert.equal(metadata.scale.y, 0.000001, label);
      assert.equal(metadata.scale.z, 0.01, label);
      assert.equal(metadata.crsWktProvenance?.selected, "vlr", label);
      assert.match(metadata.wkt, /^GEOGCS\["WGS 84"/u, label);
      assert.deepEqual(
        metadata.extraDimensions?.map(({ name, type, length, supportedScalar }) => ({
          name,
          type,
          length,
          supportedScalar,
        })),
        [{ name: "survey_code", type: "unsigned", length: 2, supportedScalar: true }],
        label,
      );

      const root = await source.loadHierarchyPage(source.getRootHierarchyPage());
      assert.equal(root.nodes.length, 1, label);
      assert.equal(root.nodes[0].key, "0-0-0-0", label);
      assert.equal(root.nodes[0].pointCount, 0, label);
      assert.equal(root.pages.length, 1, label);
      const child = await source.loadHierarchyPage(root.pages[0]);
      assert.equal(child.pages.length, 0, label);
      assert.equal(child.nodes.length, 1, label);
      const node = child.nodes[0];
      assert.equal(node.key, "1-1-1-1", label);
      assert.equal(node.pointCount, 4, label);
      assert.equal(node.pointDataLength, 208, label);

      const prepared = await source.loadPreparedPointData(
        node,
        new Set(["position", "intensity", "classification", "rgb"]),
      );
      assert.equal(prepared.pointCount, 4, label);
      assertArrayNear(Array.from(prepared.source.coordinates), [
        -123.06, 44.052, 129.59,
        -123.06001, 44.052005, 120,
        -123.05999, 44.051995, 135.59,
        -123.060005, 44.05201, 95.87,
      ], 1e-12, `${label} source XYZ`);
      assertArrayNear(Array.from(prepared.geographic.coordinates), [
        -123.06, 44.052, 130.84,
        -123.06001, 44.052005, 121.25,
        -123.05999, 44.051995, 136.84,
        -123.060005, 44.05201, 97.12,
      ], 1e-12, `${label} WGS 84 geographic`);
      assert.deepEqual(Array.from(prepared.attributes.intensity), [1001, 2002, 3003, 4004], label);
      assert.deepEqual(Array.from(prepared.attributes.classification), [2, 5, 6, 7], label);
      assert.deepEqual(Array.from(prepared.attributes.red), [1000, 4000, 7000, 10000], label);
      assert.deepEqual(Array.from(prepared.attributes.green), [2000, 5000, 8000, 11000], label);
      assert.deepEqual(Array.from(prepared.attributes.blue), [3000, 6000, 9000, 12000], label);
      assert.deepEqual(
        Array.from(prepared.attributes.extraDimensions.survey_code),
        [65000, 42, 17, 65535],
        label,
      );
      assert.equal(prepared.world.coordinateSystem, "wgs84-ecef-meters", label);
      assert.equal(prepared.world.coordinates.length, 12, label);
      assertArrayNear(Array.from(prepared.world.coordinates), [
        -2504771.1893853168, -3848181.0467095138, 4412337.011827192,
        -2504767.8903539027, -3848174.509252852, 4412330.743093421,
        -2504773.0808912846, -3848185.421731871, 4412340.784393692,
        -2504757.883204286, -3848159.869378733, 4412314.364535574,
      ], 2e-6, `${label} WGS 84 ECEF`);
      assert.ok(prepared.statistics.elevation.min > 97.11, label);
      assert.ok(prepared.statistics.elevation.max < 136.85, label);
    } finally {
      source.destroy?.();
    }
  }
});

test("Rust-only backend reports malformed input without switching implementations", async () => {
  const malformed = new Uint8Array(375);
  const backend = new RustCopcBackend({
    createByteSource: (source) => new InMemoryByteSource(malformed, source),
  });

  await assert.rejects(
    () => backend.open("memory://rust-only-malformed-header.copc.laz"),
    (error) =>
      error instanceof CopcBackendError &&
      error.stage === "metadata" &&
      error.code === "header-parse" &&
      error.cause?.code === "invalid-header",
  );
});

test(
  "bounded remote Rust Range probe reads Autzen and a SoFi child page",
  {
    skip: process.env.COPC_RUST_ONLY_REMOTE !== "1",
    timeout: 120_000,
  },
  async () => {
    const datasets = [
      {
        name: "Autzen",
        url: "https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz",
        pointFormat: 7,
        targetPointCount: 50_000,
      },
      {
        name: "SoFi",
        url: "https://hobu-lidar.s3.amazonaws.com/sofi.copc.laz",
        pointFormat: 6,
        targetPointCount: 10_000,
      },
    ];
    const maxRangeBytes = 16 * 1024 * 1024;

    for (const dataset of datasets) {
      const ranges = [];
      const measurements = [];
      let phase = "open";
      let sizeRequests = 0;
      let pointFormat;
      const httpSource = new HttpRangeByteSource(dataset.url, {
        rangeValidation: "strict",
        maxRangeBytes,
      });
      const recordingSource = {
        source: dataset.url,
        size() {
          sizeRequests += 1;
          return httpSource.size();
        },
        async readRange(offset, length, options) {
          assert.ok(length <= maxRangeBytes, `${dataset.name} Range size is bounded`);
          const startedAt = performance.now();
          const bytes = await httpSource.readRange(offset, length, options);
          if (offset === 0 && bytes.byteLength >= 105) {
            pointFormat = bytes[104] & 0x3f;
          }
          ranges.push({ phase, offset, length: bytes.byteLength });
          measurements.push({ phase, durationMs: performance.now() - startedAt });
          return bytes;
        },
        async readRanges(requests, options) {
          return Promise.all(
            requests.map(({ offset, length }) =>
              this.readRange(offset, length, options),
            ),
          );
        },
      };
      const backend = new RustCopcBackend({
        createByteSource: () => recordingSource,
      });
      const openStartedAt = performance.now();
      const source = await backend.open(dataset.url, {
        maxRangeBytes,
        rangeValidation: "strict",
      });
      const openDurationMs = performance.now() - openStartedAt;
      source.setPerformanceObserver((event) => measurements.push(event));

      try {
        const metadata = source.getMetadata();
        phase = "root-page";
        const root = await source.loadHierarchyPage(source.getRootHierarchyPage());
        let nodes = root.nodes.filter((node) => node.pointCount > 0);
        let childPage;
        if (dataset.name === "SoFi") {
          assert.ok(root.pages.length > 0, "SoFi root points to lazy child hierarchy pages");
          phase = "child-page";
          childPage = await source.loadHierarchyPage(root.pages[0]);
          nodes = childPage.nodes.filter((node) => node.pointCount > 0);
        }
        const node = nodes
          .filter((entry) => entry.pointDataLength <= maxRangeBytes)
          .sort(
            (left, right) =>
              Math.abs(left.pointCount - dataset.targetPointCount) -
              Math.abs(right.pointCount - dataset.targetPointCount),
          )[0];
        assert.ok(node, `${dataset.name} has a selected bounded point node`);

        phase = "first-point-read";
        const pointReadStartedAt = performance.now();
        const prepared = await source.loadPreparedPointData(
          node,
          new Set(["position", "intensity", "classification", "rgb"]),
        );
        const firstPointReadMs = performance.now() - pointReadStartedAt;
        phase = "repeat-point-read";
        const repeatStartedAt = performance.now();
        await source.loadPreparedPointData(
          node,
          new Set(["position", "intensity", "classification", "rgb"]),
        );
        const repeatPointReadMs = performance.now() - repeatStartedAt;

        const attributes = prepared.attributes;
        assert.ok(attributes?.intensity?.length === prepared.pointCount);
        assert.ok(attributes.classification?.length === prepared.pointCount);
        const rgbChannels = [attributes.red, attributes.green, attributes.blue];
        const hasRgb = rgbChannels.every((channel) => channel !== undefined);
        assert.ok(
          hasRgb || rgbChannels.every((channel) => channel === undefined),
          `${dataset.name} RGB channels are either all present or all absent`,
        );
        if (hasRgb) {
          assert.ok(rgbChannels.every((channel) => channel.length === prepared.pointCount));
        }
        assert.ok(Array.from(prepared.source.coordinates).every(Number.isFinite));
        assert.ok(Array.from(prepared.geographic.coordinates).every(Number.isFinite));
        assert.ok(Array.from(prepared.world.coordinates).every(Number.isFinite));

        const arrays = [
          prepared.source.coordinates,
          prepared.geographic.coordinates,
          prepared.world.coordinates,
          attributes.intensity,
          attributes.classification,
          ...rgbChannels.filter((channel) => channel !== undefined),
        ];
        const selectedRanges = ranges.reduce(
          (sum, range) => sum + range.length,
          0,
        );
        const fileSizeBytes = await recordingSource.size();
        const summary = {
          dataset: dataset.name,
          nodeVersion: process.version,
          fileSizeBytes,
          pointCount: metadata.pointCount,
          pointFormat,
          wktPrefix: metadata.wkt?.slice(0, 80),
          rootNodeCount: root.nodes.length,
          rootPageReferenceCount: root.pages.length,
          childPageNodeCount: childPage?.nodes.length ?? 0,
          selectedNode: {
            key: node.key,
            pointCount: node.pointCount,
            pointDataOffset: node.pointDataOffset,
            pointDataLength: node.pointDataLength,
          },
          sample: {
            sourceXYZ: Array.from(prepared.source.coordinates.slice(0, 6)),
            geographicLonLatHeight: Array.from(
              prepared.geographic.coordinates.slice(0, 6),
            ),
            ecefXYZ: Array.from(prepared.world.coordinates.slice(0, 6)),
            intensity: Array.from(attributes.intensity.slice(0, 4)),
            classification: Array.from(attributes.classification.slice(0, 4)),
            rgbAvailable: hasRgb,
            ...(hasRgb
              ? {
                  red: Array.from(attributes.red.slice(0, 4)),
                  green: Array.from(attributes.green.slice(0, 4)),
                  blue: Array.from(attributes.blue.slice(0, 4)),
                }
              : {}),
          },
          openDurationMs,
          firstPointReadMs,
          repeatPointReadMs,
          rangeRequestCount: ranges.length,
          sizeRequestCount: sizeRequests,
          rangeBytes: selectedRanges,
          maximumObservedRangeBytes: Math.max(...ranges.map(({ length }) => length)),
          rangeBytesFraction: selectedRanges / fileSizeBytes,
          preparedRetainedArrayBytes: arrays.reduce(
            (sum, array) => sum + array.byteLength,
            0,
          ),
          workerDiagnostics: source.getWorkerDiagnostics(),
          stages: measurements.filter((event) => event.stage).map((event) => ({
            stage: event.stage,
            durationMs: event.durationMs,
            bytes: event.bytes,
            blocksMainThread: event.blocksMainThread,
          })),
        };
        assert.ok(summary.rangeBytesFraction < 0.01);
        assert.ok(summary.maximumObservedRangeBytes <= maxRangeBytes);
        assert.equal(summary.pointFormat, dataset.pointFormat);
        console.log(`[rust-only-remote] ${JSON.stringify(summary)}`);
      } finally {
        source.destroy?.();
      }
    }
  },
);
