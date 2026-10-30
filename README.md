# pdfdistill-lopdf

Shrinks bloated PDFs — in particular the ones produced by **Microsoft Print to PDF**, which
embeds complete TrueType fonts and stores every image as lossless Flate — by deduplicating
objects, subsetting fonts, recompressing/downsampling images and rewriting the file structure.

This repository contains:

* a **[4D component](#4d-component)** (`pdfdistill-lopdf/`) that lets 4D applications distill
  PDFs through the class `cs.lopdf.distill`;
* the **[command-line tool](#command-line-tool)** it wraps: a small, self-contained Rust program
  built on [lopdf](https://github.com/J-F-Liu/lopdf), with only permissively licensed
  dependencies.

## 4D component

The component bundles the `pdfdistill-lopdf` executable — a signed and notarized universal
binary for macOS, and static builds for Windows x64 and ARM64 — in
`Resources/bin/{macOS,Windows,WindowsARM}` (see [CI / releases](#ci--releases)) and runs it with
`4D.SystemWorker`. If the executable for the current platform is missing, `pdfdistill-lopdf`
is looked up on the `PATH`. No plug-in is involved. It targets 4D 20 and later.

### Installation

**Project Dependencies (4D 20 R6 and later):** add the component from the *Project
Dependencies* panel (GitHub repository `miyako/pdfdistill-lopdf`), or declare it in your
project's `Project/Sources/dependencies.json`:

```json
{
  "dependencies": {
    "pdfdistill-lopdf": {
      "github": "miyako/pdfdistill-lopdf",
      "version": "^20.0.0"
    }
  }
}
```

**Manual:** download `pdfdistill-lopdf.zip` from the
[latest release](https://github.com/miyako/pdfdistill-lopdf/releases/latest) and place the
compiled component in your project's `Components` folder.

The component's classes are exposed in the `cs.lopdf` class store.

### Quick start

```4d
var $distill : cs.lopdf.distill
$distill:=cs.lopdf.distill.new()

var $version : Text
$version:=$distill.version()  // e.g. "0.1.0"

var $in; $out : 4D.File
$in:=Folder(fk desktop folder).file("report.pdf")
$out:=Folder(fk desktop folder).file("report.distilled.pdf")

// file to file, synchronous; each task is a collection of CLI arguments
$distill.execute([$in; "-o"; $out; "--quality"; 60; "--max-dpi"; 150])

// several tasks in one call, e.g. lossless only
$distill.execute([\
  [$in; "-o"; $out; "--lossless"]; \
  [Folder(fk desktop folder).file("other.pdf"); "--no-images"]])  // -> other.distilled.pdf

// Blob in (via stdin), file out
var $pdf : 4D.Blob
$pdf:=$in.getContent()
$distill.execute(["-"; "-o"; $out; {file: $pdf}])
```

Task elements are mapped to the command line by type: `4D.File` → its path, Text → an argument
(flags such as `"--lossless"`), Number → its value, and an object `{data: …; file: …}` supplies
a callback context (`data`) and/or the stdin payload (`file`, used when the input is `"-"`).
Output must go to a file (`-o` + `4D.File`): stdout is captured as text, so PDF bytes written
to stdout cannot be retrieved as a Blob.

To run asynchronously, pass an `onResponse` callback; it receives the `4D.SystemWorker`
(`exitCode`, and the CLI's summary line in `responseError`) and the event object, whose
`context` is the task's `data`. The calling process must stay alive until the callback fires:

```4d
#DECLARE($params : Object)

If ($params=Null)
	CALL WORKER(1; Current method name; {})
Else 
	var $in; $out : 4D.File
	$in:=Folder(fk desktop folder).file("report.pdf")
	$out:=Folder(fk desktop folder).file("report.distilled.pdf")
	
	cs.lopdf.distill.new().execute([[$in; "-o"; $out; {data: $out}]]; \
		{onResponse: Formula(ALERT(String($1.exitCode)+": "+$1.responseError))})
End if 
```

### Class documentation

* [`distill`](pdfdistill-lopdf/Documentation/Classes/distill.md) — public API: `version()`,
  `execute()`, `terminate()`, options, callbacks and examples
* Internal: [`_CLI`](pdfdistill-lopdf/Documentation/Classes/_CLI.md),
  [`_CLI_Controller`](pdfdistill-lopdf/Documentation/Classes/_CLI_Controller.md),
  [`_distill_Controller`](pdfdistill-lopdf/Documentation/Classes/_distill_Controller.md)

## Command-line tool

This is one of three sibling implementations (Rust/lopdf, C++/qpdf, Go/pdfcpu) sharing the same
CLI so that they can be benchmarked against each other.

### Usage

```
pdfdistill-lopdf <INPUT|-> [-o <OUTPUT|->] [options]
  INPUT  path or '-' for stdin
  -o     output path or '-' for stdout (default: stdout if input is '-', else <input>.distilled.pdf)
  --quality <1-100>      JPEG quality for recompressed images (default 75)
  --max-dpi <n>          downsample images whose effective resolution exceeds n (default 150; 0 = never)
  --no-images            skip image recompression
  --no-subset            skip font subsetting
  --no-dedup             skip duplicate object/image/font removal
  --lossless             only lossless optimizations (implies no JPEG recompression / downsampling)
  -v, --verbose          print per-step size statistics to stderr
  -q, --quiet
  --version, --help
```

* The default output name replaces a `.pdf` extension: `report.pdf` → `report.distilled.pdf`
  (a file without a `.pdf` extension gets `.distilled.pdf` appended).
* Exit code 0 on success, non-zero on error. All diagnostics go to stderr; stdout carries only
  PDF bytes, so `pdfdistill-lopdf - < in.pdf > out.pdf` is safe.
* Unless `-q` is given, one summary line is printed to stderr; `-v` adds per-step sizes.
* The output is **never larger than the input**: if optimization does not help, the original
  bytes are written unchanged. Encrypted PDFs are passed through unchanged.

```
$ pdfdistill-lopdf tests/fixtures/ms-print-to-pdf-1.pdf -v
input          720651 bytes
dedup          719751 bytes (-0.1%)  merged 0 objects
fonts          583204 bytes (-19.0%)  subset 1 font(s), skipped 0; font programs 143832 -> 7052 bytes
images         114991 bytes (-80.3%)  2 to JPEG, 2 re-deflated, 0 downsampled; 566511 -> 98304 bytes
structure      114690 bytes (-0.3%)  recompressed 1 streams, pruned 1 objects
pdfdistill-lopdf: 720651 -> 114690 bytes (84.1% smaller) -> tests/fixtures/ms-print-to-pdf-1.distilled.pdf [0.16s]
```

(The per-step sizes in `-v` mode are measured by serializing the document after each step.)

#### Results on the fixture (`tests/fixtures/ms-print-to-pdf-1.pdf`, 720,651 bytes)

| options | output bytes | reduction |
|---|---:|---:|
| *(defaults)* | 114,690 | 84.1% |
| `--quality 50` | 95,467 | 86.8% |
| `--quality 90` | 187,410 | 74.0% |
| `--max-dpi 36` | 53,163 | 92.6% |
| `--lossless` | 572,020 | 20.6% |
| `--no-subset` | 250,399 | 65.3% |
| `--no-images --no-subset --no-dedup` (structure only) | 707,729 | 1.8% |

Rendered with Ghostscript and macOS Quartz (`sips`), the default output is visually identical
to the input apart from JPEG artifacts (mean absolute pixel difference ≈0.3/255); with
`--lossless` the renders are bit-identical.

### What it does

Passes run in this order:

1. **Dedup** — every stream is hashed (SHA-256) over its *decoded* content plus its dictionary
   minus encoding keys (`Length`, `Filter`, `DecodeParms`, `DL`); `Font`, `FontDescriptor`,
   `ExtGState`, `Encoding` dictionaries and indirect colour-space arrays are hashed canonically.
   Duplicates are merged into the smallest encoding and references are rewritten. This repeats
   until a fixed point, so SMasks merge first and then the images that use them become equal.
2. **Font subsetting** — Type0 fonts with `Identity-H`/`Identity-V` encoding and a
   `CIDFontType2` descendant with `FontFile2` (exactly what Microsoft Print to PDF emits).
   Used CIDs are collected from every page content stream, nested Form XObjects, tiling
   patterns and annotation appearance streams (`/AP /N /R /D`). The font program is subset
   with Typst's [`subsetter`](https://github.com/typst/subsetter) (keeps `.notdef` and
   composite-glyph components); because glyph ids are renumbered, a new `CIDToGIDMap` stream is
   written — content streams are never rewritten. `/W` is trimmed to the used CIDs, `/CIDSet`
   is dropped and `BaseFont`/`FontName` get a deterministic `ABCDEF+` subset tag. A font is
   only subset if *every* object that references it was analysed successfully (fonts also
   reachable from e.g. AcroForm `/DR`, Type3 glyph procedures or unparsable content are left
   alone).
3. **Images** — 8-bit DeviceGray/DeviceRGB (also ICCBased N=1/3, CalGray, CalRGB) images
   stored losslessly are re-encoded as baseline JPEG (`DCTDecode`, optimized Huffman tables)
   at `--quality` if that is smaller. SMasks and stencil `/Mask` images stay Flate (level 9).
   Images are downsampled (area-average filter) when their effective resolution exceeds
   `--max-dpi`; the effective resolution is computed from the CTM at every `Do` (largest
   placement wins) and SMasks follow their parent image's placement. Existing JPEGs are only
   decoded/re-encoded when they must be downsampled. Skipped: `/ImageMask`, Indexed, 1/2/4/16-bit,
   non-default `/Decode`, colour-key `/Mask` arrays, CMYK (Flate-recompressed only),
   `/SMaskInData`. Images whose placement cannot be determined are never downsampled.
   Every new JPEG is verified by decoding it before use.
4. **Structure** — unreachable objects are removed, every decodable stream (Flate, LZW,
   ASCII85, ASCIIHex, RunLength) is re-deflated at level 9 if that is smaller (XMP metadata is
   left as is), objects are renumbered densely, and the file is written with object streams and
   a cross-reference stream (PDF 1.5+). The `/Info` dictionary is preserved. The output is
   re-parsed as a sanity check; if that fails the original is returned.

### Building

Requires Rust 1.88+.

```sh
cargo build --release          # target/release/pdfdistill-lopdf
cargo test --release           # unit + integration tests (incl. size regression)
```

#### macOS universal binary (arm64 + x86_64)

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create -output pdfdistill-lopdf \
  target/aarch64-apple-darwin/release/pdfdistill-lopdf \
  target/x86_64-apple-darwin/release/pdfdistill-lopdf
otool -L pdfdistill-lopdf      # only /usr/lib/libSystem and /usr/lib/libiconv
```

#### Windows (static CRT)

`.cargo/config.toml` sets `-C target-feature=+crt-static` for the MSVC targets, so a plain
release build has no VC++ redistributable dependency:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc   # optional
```

All dependencies are pure Rust (zlib-rs backend for Flate), so no C toolchain or system
libraries are needed beyond the platform linker.

#### CI / releases

`.github/workflows/build.yml` (Build) checks dependency licenses with `cargo-deny`, runs the
tests, builds a universal macOS binary (via `lipo`) and Windows x64/arm64 binaries (static CRT,
verified with `dumpbin /DEPENDENTS`), and uploads them as artifacts. Pull requests only build
and test.

On push to `main` (changes outside `pdfdistill-lopdf/`) or a manual run on `main`, Build also:

1. code signs the macOS binary (Developer ID, hardened runtime) and notarizes it with
   `notarytool` (a bare Mach-O can't be stapled, so the ticket is fetched online by Gatekeeper);
2. copies the binaries into the 4D component —
   `pdfdistill-lopdf/Resources/bin/macOS/pdfdistill-lopdf`,
   `pdfdistill-lopdf/Resources/bin/Windows/pdfdistill-lopdf.exe`,
   `pdfdistill-lopdf/Resources/bin/WindowsARM/pdfdistill-lopdf.exe` — and commits them to `main`
   as `github-actions[bot]` if they changed;
3. dispatches the Publish workflow (`publish.yml`, patch bump), which builds, signs and
   releases the 4D component from that commit.

Signing uses the same repository secrets as Publish (`APPLE_DEVELOPER_ID_CERTIFICATE`,
`APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD`, `CODESIGN_APPLE_ID`,
`NOTARYTOOL_APPLE_ID`, `NOTARYTOOL_TEAM_ID`, `NOTARYTOOL_PASSWORD`).

Pushing a `v*` tag creates a GitHub release with the zipped CLI binaries.

### Dependencies and licenses

Only permissive licenses are allowed (enforced by `deny.toml`). Direct dependencies:

| crate | purpose | license |
|---|---|---|
| [lopdf](https://crates.io/crates/lopdf) | PDF parsing/writing | MIT |
| [flate2](https://crates.io/crates/flate2) + [zlib-rs](https://crates.io/crates/zlib-rs) | Deflate | MIT/Apache-2.0, Zlib |
| [subsetter](https://crates.io/crates/subsetter) | TrueType/OpenType subsetting | MIT/Apache-2.0 |
| [jpeg-encoder](https://crates.io/crates/jpeg-encoder) | JPEG encoding | (MIT/Apache-2.0) AND IJG |
| [zune-jpeg](https://crates.io/crates/zune-jpeg), zune-core | JPEG decoding (downsampling existing JPEGs, verification) | MIT/Apache-2.0/Zlib |
| [sha2](https://crates.io/crates/sha2) | hashing for dedup | MIT/Apache-2.0 |

Transitive dependencies are MIT, Apache-2.0, BSD-2/3-Clause, 0BSD, Zlib, Unlicense (dual with
MIT) or Unicode-3.0. Run `cargo deny check licenses` to verify.

### Limitations

* Only `Identity-H/V` CIDFontType2 fonts are subset; simple TrueType/Type1 and CFF fonts are
  left as is (they are still deduplicated and recompressed).
* Effective DPI ignores clipping (an image drawn larger than its visible area keeps the
  resolution of its full placement).
* Encrypted documents are not modified.

## License

MIT — see [LICENSE](LICENSE).
