import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { RustCopcBackend } from "../../../../core/copc/backend/rustCopcBackend.ts";
import { InMemoryByteSource } from "../../../../core/copc/range/inMemoryByteSource.ts";
import { CopcStreamingCore } from "../../../../core/streaming/CopcStreamingCore.ts";

const fixturePath = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../../../../../crates/core/tests/fixtures/rust-only-golden-pf8.copc.laz",
);

test("Rust-only source contract feeds the public bounded spatial query", async () => {
  const fixture = await readFile(fixturePath);
  const backend = new RustCopcBackend({
    createByteSource: (source) => new InMemoryByteSource(fixture, source),
  });
  const core = new CopcStreamingCore({
    url: "memory://rust-only-golden-pf8.copc.laz",
    backend,
    pointFields: new Set(["position", "intensity", "classification", "rgb"]),
    streaming: {
      maxDepth: 1,
      maxNodes: 8,
      maxRenderedPoints: 16,
      maxRenderDistanceMeters: 1_000,
      maxScreenSpaceError: 8,
      maxConcurrentNodeLoads: 1,
    },
  });

  try {
    await core.load();
    const cube = core.getMetadata().cube;
    const query = core.queryPoints({
      region: {
        coordinateSystem: "copc-source",
        min: { x: cube.minX, y: cube.minY, z: cube.minZ },
        max: { x: cube.maxX, y: cube.maxY, z: cube.maxZ },
      },
      mode: "fetch-missing",
      maxDepth: 1,
      maxPoints: 8,
      maxBytes: 16 * 1024,
      maxHierarchyPages: 4,
      maxDurationMs: 5_000,
    });
    const points = [];
    for await (const batch of query) points.push(...batch);
    const summary = await query.result;

    assert.equal(summary.status, "complete");
    assert.equal(summary.nodesProcessed, 1);
    assert.equal(summary.pointsScanned, 4);
    assert.equal(summary.pointsReturned, 4);
    const expected = [
      [-123.06, 44.052, 129.59],
      [-123.06001, 44.052005, 120],
      [-123.05999, 44.051995, 135.59],
      [-123.060005, 44.05201, 95.87],
    ];
    for (const [index, point] of points.entries()) {
      assert.equal(point.nodeKey, "1-1-1-1");
      assert.equal(point.pointIndex, index);
      for (const [component, value] of [point.x, point.y, point.z].entries()) {
        assert.ok(Math.abs(value - expected[index][component]) <= 1e-12);
      }
    }
  } finally {
    core.destroy();
  }
});
