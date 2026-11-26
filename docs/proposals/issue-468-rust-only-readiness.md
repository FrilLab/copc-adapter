# Issue #468: Rust-only COPC readiness and release gates

**Status:** Evidence and CI gates are prepared for maintainer GO/NO-GO review.
This document does not approve a default-backend change or declare Rust-only
production readiness. The production default remains `copc-js`; Rust stays
opt-in.

Issue [#468](https://github.com/FrilLab/copc-adapter/issues/468) asks for an
independent conformance oracle, real-data range and security evidence, packed
consumer checks, and an enforced readiness gate. This page records the
supported contract, evidence, and limits so a maintainer can make that
decision.

## Public contract and capability matrix

| Area | Rust backend contract | Evidence and limits |
| --- | --- | --- |
| Selection and fallback | Callers may select `RustCopcBackend`; it does not switch to `CopcJsBackend` after a Rust error. The package default is unchanged. | Direct malformed-header test asserts a structured `CopcBackendError`; public `CopcStreamingCore` query test uses Rust explicitly. |
| Inputs and range reads | URL sources use bounded HTTP Range reads; `Blob`, `File`, and custom `RandomAccessByteSource` inputs are supported. | The checked-in fixture is read through all three non-URL input forms. Live Autzen and SoFi probes use strict HTTP Range validation. |
| LAS point formats | Formats 6, 7, and 8 are supported. Format 6 has no RGB; formats 7 and 8 expose RGB. Format 8 NIR is not exposed as a public prepared attribute. | Independent PF8 fixture; live Autzen PF7 and SoFi PF6. No claim for other formats. |
| Attributes | Position, intensity, classification, and RGB where present are prepared. Scalar Extra Bytes data types 1–10 can be decoded when requested. | Golden fixture checks intensity, classification, RGB, and U16 `survey_code`. Vector Extra Bytes types 11–30 and untyped byte fields are metadata-only; reserved types are rejected. |
| Hierarchy and query | Root and lazy child hierarchy pages feed the bounded public spatial query. Query limits include depth, nodes, points, bytes, pages, and duration. | Fixture has an empty root node, a child-page reference, and a point node. The public query checks all four points and their node/point provenance. SoFi provides a real lazy child page. |
| CRS and height | Rust-only preparation uses the documented supported CRS subset and shared explicit vertical correction path. It does not fall back to JavaScript projection. | See [issue #244](issue-244-vertical-crs-geoid-grid.md) for supported WKT shapes, vertical operations, and grid limitations. General PROJ/WKT support and inferred vertical-datum conversion are not claimed. |
| Workers and renderers | Rust can run in a Worker where available and falls back to main-thread execution without a Worker. The packed Rust path is exercised with Cesium and Three.js consumers. | Packed Cesium browser coverage checks no-Worker execution, WASM assets, unload/reload, and rendering. Packed Three.js smoke coverage includes Rust rendering and picking. Main-thread decode can block. |
| Errors and malformed data | Parse, hierarchy, decode, and source failures are surfaced as structured errors with bounded allocation/read limits; errors do not select a different backend. | Existing header, VLR/EVLR, hierarchy, Extra Bytes, LAZ chunk, worker lifecycle/failure, and source error tests remain in their own validation layers. See test map below. |

The Rust backend does not expose a point-record editing API. Styling and
filtering operate on prepared/rendered points and do not mutate source records.
The compatibility matrix is intentionally limited to the behavior tested in
this repository; it is not a claim of full COPC or PROJ conformance.

## Independent golden oracle

`crates/core/tests/fixtures/rust-only-golden-pf8.copc.laz` is a checked-in
LAS 1.4 / COPC fixture with WKT1 WGS 84, PF8 records, a scalar U16 Extra Bytes
dimension, a root-to-child hierarchy reference, and four explicitly specified
point records. The fixture generator uses the Rust `laz` compressor, but its
header, expected coordinates, fields, and hierarchy are specified directly;
the expected values are not obtained from `copc-js`. Rust tests derive source
XYZ from LAS integers, scale, and offset, and compare ECEF against a separate
WGS 84 ellipsoid formula. Adapter tests read the same committed binary through
custom-memory, `Blob`, and `File` sources and check the public spatial-query
contract. The generator instructions and fixture contents are recorded in
`crates/core/tests/fixtures/README.md`.

## Real COPC bounded-range evidence

The optional command `npm run test:rust-only:remote -w @frillab/copc-adapter`
uses strict HTTP Range validation, reads the root page, reads a SoFi child
page, reads one representative point node, and repeats that node read. It is
not part of CI because it depends on public S3 availability and live content.
The latest run was on 2026-11-26 with Node.js 26.12.1, Rust 1.92.0, and an
arm64 Apple M2 running macOS. Timings below are one run, not a benchmark
series.

| Dataset | File / points | LAS format and hierarchy | Selected node | Range bytes / file | Largest range | Open / first prepared read / repeat |
| --- | ---: | --- | --- | ---: | ---: | ---: |
| Autzen | 81,123,042 B / 10,653,336 | PF7; 278 root point nodes, no child page references | `4-8-10-0`, 49,860 points, 313,898 B | 638,863 B / 0.7875% | 313,898 B | 1,649 / 989 / 1,232 ms |
| SoFi | 2,029,696,615 B / 364,384,576 | PF6; 2,599 root nodes, 111 child page references; selected child page has 87 nodes | `7-24-24-8`, 16,578 points, 78,746 B | 248,976 B / 0.01227% | 86,720 B | 1,722 / 496 / 485 ms |

Both datasets returned finite source, geographic, and ECEF coordinates plus
intensity and classification. Autzen returned RGB arrays; SoFi PF6 did not.
The prepared typed arrays reported by the probe retained 4,038,660 bytes for
the selected Autzen node and 1,243,350 bytes for SoFi. This sums the exposed
source, geographic, ECEF, and requested standard attribute arrays; it is not
peak process memory or total live WASM/Worker memory. The current probe does
not measure transient allocator usage, browser memory, or SoFi browser CORS.
Node confirmed strict partial Range responses for both public S3 objects.
Both probe runs used the Rust backend without a Worker, so decode and point
preparation were reported as main-thread work.

## Security, failure, and consumer gates

Relevant coverage is deliberately kept at separate validation boundaries:

- Native Rust tests cover header and metadata parsing, VLR/EVLR limits,
  hierarchy structure and limits, Extra Bytes shape/type checks, CRS errors,
  LAZ formats 6/7/8, malformed chunks, and decode limits.
- Adapter tests cover source/header/chunk error mapping, malformed lazy
  hierarchy pages, bounded query behavior, and the no-fallback contract.
- Worker-pool tests cover queue concurrency and FIFO, superseded work, query
  abort, worker/init failure, and idempotent destroy.
- Browser and packed-consumer suites cover external package resolution,
  Rust/WASM assets, rendering/picking, and lifecycle. The new packed Cesium
  case makes `Worker` unavailable and checks unload followed by reload.
- The packed Three.js smoke test checks the separately packaged Three consumer;
  the renderer-free packed Core smoke test checks the NodeNext consumer.

The required `rust-only-readiness` CI job runs the independent native Rust
golden test and the direct Rust/WASM golden plus public query tests. The
existing final required `viewer-web` status check now requires that job along
with Rust, fast adapter, browser, package-build, and packed-consumer jobs.
Network-dependent Autzen/SoFi probes remain opt-in and do not make CI depend on
external datasets.

## Maintainer decision and limitations

This record supplies release evidence; it does not set the maintainer
GO/NO-GO. A GO would still need to state which supported contract is
acceptable for a Rust-only release and whether the documented unsupported
features are acceptable. No default switch is included here. The measured
range bytes and output-array sizes are evidence for the selected nodes only;
they do not establish whole-scene memory ceilings, cross-machine performance,
browser CORS for SoFi, or latency under concurrent users.

## Validation record

Validation completed locally on macOS arm64 with Node.js 26.12.1 and Rust
1.92.0:

- `cargo fmt --all -- --check` and `git diff --check` passed.
- `cargo test --workspace` passed: 54 core unit tests, 1 independent golden
  integration test, 8 WASM unit tests, and 4 CRS audit tests.
- `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `npm run typecheck -w @frillab/copc-adapter` passed.
- `npm test -w @frillab/copc-adapter` passed: 686 tests passed and the
  opt-in live remote probe was skipped.
- `npm run test:rust-only:remote -w @frillab/copc-adapter` passed all 4 tests,
  including the live Autzen and SoFi Range probes summarized above.
- `npm run build:library:ci -w @frillab/copc-adapter` and
  `npm run docs:build` passed. VitePress reported only its configured large
  chunk size advisory.
- The packed root Cesium production consumer completed its Chromium suite with
  14 passed and one live imagery case skipped, including the no-Worker reload
  regression. The packed Three.js consumer passed build, type checks, render,
  pick, and destroy checks; it rendered 219,951 points across 7 nodes and used
  9 HTTP 206 range requests.

CI remains the enforced cross-platform release gate.
