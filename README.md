# flimg

Lossless JPEG/PNG preprocessor for general-purpose archivers (zpaq, 7z, zstd, etc.).

Zero dependencies on preflate-rs container crate — uses the preflate core directly
for PNG, and Lepton for JPEG. No Zstd layer, no patched libraries. Output is raw
pixel data that any archiver can compress efficiently.

## Architecture

| Format | Method | Dependency |
|--------|--------|-----------|
| JPEG | Lepton re-encoding (~22% reduction, byte-identical) | `lepton_jpeg` |
| PNG | Manual IDAT parsing → preflate core DEFLATE decompression → raw pixel storage (byte-identical) | `preflate-rs` core only |

Unlike [`rawflate`](https://github.com/innocence100/rawflate), flimg does NOT use `preflate-container`. It parses PNG IDAT
chunks manually and calls `preflate_whole_deflate_stream` directly on the
extracted DEFLATE data. Reconstruction uses `recreate_whole_deflate_stream` +
hand-built IDAT chunks + CRC-32.

No Zstd anywhere in the pipeline. The `.raw` files contain raw plaintext +
corrections — directly compressible by zpaq.

| | rawflate | flimg |
|---|---|---|
| preflate-rs dep | container crate (fork + 3 patches) | **core crate direct** (upstream v0.7.6, zero patches) |
| Zstd | `PreflateContainerProcessor` + `no_zstd=true` | **nonexistent** |
| PNG handling | container scans IDAT + pre-decompression | **hand-written IDAT parser** (~30 lines) |
| JPEG | lepton_jpeg | same |
| CI | clones fork to ../preflate-rs | **zero clone step** |
| dependency count | container + zstd + preflate core | **only preflate core + lepton_jpeg** |

## Usage

```bash
flimg -m encode -i photo.jpg -o photo.jpg.raw
flimg -m decode -i photo.jpg.raw -o restored.jpg
flimg -m encode -i icon.png  -o icon.png.raw
flimg -m decode -i icon.png.raw  -o restored.png
```

## Building

Requires Rust ≥ 1.89.

```bash
cargo build --release
```