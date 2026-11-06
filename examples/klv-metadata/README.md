# KLV metadata

ST 0601 / ST 0102 / ST 0903 typed encode + decode. Five examples; the
middle two form an explicit two-step pipeline (extract → decode).

## 1. `klv_encode_minimal.rs` — start here for encoding

```sh
cargo run -p tst-examples --example klv_encode_minimal
```

Build the smallest valid ST 0601 record: timestamp + UAS LS version,
encode to bytes, print hex. The encoder side of the substrate.

Cookbook: [Encode ST 0601 from typed values](../../docs/cookbook/klv/encode-st0601.md).

## 2. `extract_klv.rs` — extract KLV from a `.ts` file

```sh
cargo run -p tst-examples --example extract_klv -- path/to/capture.ts /tmp/klv_out
```

Run the demuxer over a `.ts` file, write each KLV record to its own
`.klv` blob on disk. Pure extraction, no parsing. Forms a 2-step
pipeline with §3.

## 3. `klv_decode_file.rs` — decode a `.klv` blob

```sh
cargo run -p tst-examples --example klv_decode_file -- path/to/record.klv
```

Read one `.klv` blob produced by §2 (or any other source), decode it
through the strictness ladder (`decode_strict_compliance` / `decode_strict` /
`decode` / `decode_unchecked`), print the typed fields.

Cookbook: [Decode ST 0601 from a captured `.klv` blob](../../docs/cookbook/klv/decode-st0601-blob.md).

## 4. `decode_security_metadata.rs` — ST 0102 (Security LS)

```sh
cargo run -p tst-examples --example decode_security_metadata -- path/to/file.ts
```

Diff from §3: ST 0102 instead of ST 0601. The Security LS lives at
ST 0601 Tag 48 (pass-through) and is decoded by a sibling-layer parser
in `klv::st0102`.

Cookbook: [Decode security metadata from an ST 0601 record](../../docs/cookbook/klv/decode-security-metadata.md).

## 5. `decode_vmti_metadata.rs` — ST 0903 (VMTI LS)

```sh
cargo run -p tst-examples --example decode_vmti_metadata -- path/to/capture.ts
```

ST 0903 Video Moving Target Indicator. Sibling-layer to ST 0601 (Tag 74
pass-through) with a richer nested structure: `VTargetSeries` of
`VTargetPack` records with optional sub-LSes. Demonstrates the
`Encoding::VarUint` substrate.

Cookbook: [Decode VMTI per-target detections from an ST 0601 stream](../../docs/cookbook/klv/decode-vmti.md).
