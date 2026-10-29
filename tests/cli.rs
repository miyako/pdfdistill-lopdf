//! End-to-end tests against the Microsoft Print to PDF fixture.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use lopdf::{Document, Object};
use pdfdistill::util::{decode_stream, resolve};

const BIN: &str = env!("CARGO_BIN_EXE_pdfdistill-lopdf");
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/ms-print-to-pdf-1.pdf"
);

fn run_stdin(args: &[&str], input: &[u8]) -> (i32, Vec<u8>, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    (
        out.status.code().unwrap_or(-1),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn distill(args: &[&str]) -> Vec<u8> {
    let input = std::fs::read(FIXTURE).unwrap();
    let mut a = vec!["-", "-q"];
    a.extend_from_slice(args);
    let (code, out, err) = run_stdin(&a, &input);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(err.is_empty(), "quiet mode must not print: {err}");
    out
}

struct ImageInfo {
    filter: String,
    has_smask: bool,
}

fn images(doc: &Document) -> Vec<ImageInfo> {
    doc.objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| s.dict.get(b"Subtype").and_then(|n| n.as_name()).ok() == Some(b"Image"))
        .map(|s| ImageInfo {
            filter: s
                .dict
                .get(b"Filter")
                .and_then(|f| f.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default(),
            has_smask: s.dict.has(b"SMask"),
        })
        .collect()
}

/// For every Type0 font: (BaseFont, used CIDs, CIDs that map to a real glyph in the embedded font).
fn font_report(doc: &Document) -> Vec<(String, usize, usize)> {
    let usage = pdfdistill::content::analyse(doc);
    let mut out = Vec::new();
    for (id, obj) in &doc.objects {
        let Ok(d) = obj.as_dict() else { continue };
        if d.get(b"Subtype").and_then(|s| s.as_name()).ok() != Some(b"Type0") {
            continue;
        }
        let name =
            String::from_utf8_lossy(d.get(b"BaseFont").unwrap().as_name().unwrap()).into_owned();
        let cids: BTreeSet<u16> = usage.font_codes.get(id).cloned().unwrap_or_default();
        let Object::Array(df) = resolve(doc, d.get(b"DescendantFonts").unwrap()) else {
            panic!()
        };
        let cf = resolve(doc, &df[0]).as_dict().unwrap();
        let fd = resolve(doc, cf.get(b"FontDescriptor").unwrap())
            .as_dict()
            .unwrap();
        let ff = resolve(doc, fd.get(b"FontFile2").unwrap())
            .as_stream()
            .unwrap();
        let font = decode_stream(ff).unwrap();
        let face = ttf_glyph_count(&font);
        let map: Option<Vec<u16>> = match cf.get(b"CIDToGIDMap").map(|o| resolve(doc, o)) {
            Ok(Object::Stream(s)) => Some(
                decode_stream(s)
                    .unwrap()
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect(),
            ),
            _ => None,
        };
        let mapped = cids
            .iter()
            .filter(|&&c| {
                let g = map
                    .as_ref()
                    .map(|m| m.get(c as usize).copied().unwrap_or(0))
                    .unwrap_or(c);
                g != 0 && g < face
            })
            .count();
        out.push((name, cids.len(), mapped));
    }
    out
}

fn ttf_glyph_count(data: &[u8]) -> u16 {
    let n = u16::from_be_bytes([data[4], data[5]]) as usize;
    for i in 0..n {
        let rec = 12 + i * 16;
        if &data[rec..rec + 4] == b"maxp" {
            let off = u32::from_be_bytes(data[rec + 8..rec + 12].try_into().unwrap()) as usize;
            return u16::from_be_bytes([data[off + 4], data[off + 5]]);
        }
    }
    panic!("no maxp")
}

#[test]
fn default_settings_hit_size_target_and_stay_valid() {
    let input = std::fs::read(FIXTURE).unwrap();
    let out = distill(&[]);
    assert!(out.starts_with(b"%PDF-"));
    // Size regression: the reference result is ~115 KB; allow some slack for encoder changes.
    assert!(out.len() < 140_000, "output too large: {} bytes", out.len());
    assert!(out.len() < input.len() / 4);

    let before = Document::load_mem(&input).unwrap();
    let after = Document::load_mem(&out).unwrap();
    assert_eq!(after.get_pages().len(), before.get_pages().len());

    // Images: still four (two RGB + two SMasks); RGB recompressed as JPEG, SMasks stay Flate.
    let imgs = images(&after);
    assert_eq!(imgs.len(), 4);
    assert_eq!(imgs.iter().filter(|i| i.filter == "DCTDecode").count(), 2);
    assert!(imgs
        .iter()
        .filter(|i| i.has_smask)
        .all(|i| i.filter == "DCTDecode"));
    assert!(imgs
        .iter()
        .filter(|i| !i.has_smask)
        .all(|i| i.filter == "FlateDecode"));

    // Fonts: same glyphs used, all of them present in the subset font, name tagged.
    let fb = font_report(&before);
    let fa = font_report(&after);
    assert_eq!(fb.len(), 1);
    assert_eq!(fa.len(), 1);
    assert_eq!(fa[0].1, fb[0].1, "used glyph count changed");
    assert_eq!(
        fa[0].2, fa[0].1,
        "some used glyphs are missing from the subset font"
    );
    let name = &fa[0].0;
    assert_eq!(
        name.as_bytes()[6],
        b'+',
        "BaseFont lacks subset tag: {name}"
    );
    assert!(name[..6].bytes().all(|c| c.is_ascii_uppercase()));
}

#[test]
fn stdout_is_clean_and_deterministic() {
    let a = distill(&[]);
    let b = distill(&[]);
    assert_eq!(a, b);
}

#[test]
fn lossless_keeps_images_flate() {
    let out = distill(&["--lossless"]);
    let doc = Document::load_mem(&out).unwrap();
    assert!(images(&doc).iter().all(|i| i.filter == "FlateDecode"));
    assert!(out.len() < 600_000);
}

#[test]
fn downsampling_reduces_further() {
    let def = distill(&[]);
    let low = distill(&["--max-dpi", "36"]);
    assert!(low.len() < def.len());
    let doc = Document::load_mem(&low).unwrap();
    for s in doc.objects.values().filter_map(|o| o.as_stream().ok()) {
        if s.dict.get(b"Subtype").and_then(|n| n.as_name()).ok() == Some(b"Image") {
            assert_eq!(s.dict.get(b"Width").unwrap().as_i64().unwrap(), 256);
        }
    }
}

#[test]
fn option_toggles() {
    let no_subset = distill(&["--no-subset", "--no-images"]);
    let doc = Document::load_mem(&no_subset).unwrap();
    let fr = font_report(&doc);
    assert_eq!(
        fr[0].0, "CIDFont+F1",
        "--no-subset must leave the font untouched"
    );
    assert!(images(&doc).iter().all(|i| i.filter == "FlateDecode"));
    let q = distill(&["--quality", "30"]);
    assert!(q.len() < distill(&[]).len());
}

#[test]
fn never_larger_than_input() {
    let once = distill(&[]);
    let (code, twice, _) = run_stdin(&["-", "-q"], &once);
    assert_eq!(code, 0);
    assert!(twice.len() <= once.len());
}

#[test]
fn file_output_default_name() {
    let dir = std::env::temp_dir().join(format!("pdfdistill-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let inp = dir.join("sample.pdf");
    std::fs::copy(FIXTURE, &inp).unwrap();
    let out = Command::new(BIN).arg(&inp).arg("-v").output().unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "nothing must go to stdout when writing a file"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("fonts") && err.contains("images") && err.contains("structure"),
        "{err}"
    );
    let produced = dir.join("sample.distilled.pdf");
    assert!(Document::load(&produced).is_ok());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn errors_are_reported_with_nonzero_exit() {
    let (code, out, err) = run_stdin(&["-"], b"this is not a pdf");
    assert_ne!(code, 0);
    assert!(out.is_empty());
    assert!(err.contains("error"));

    let o = Command::new(BIN)
        .args(["x.pdf", "--quality", "101"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let o = Command::new(BIN)
        .args(["x.pdf", "--bogus"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    let o = Command::new(BIN)
        .arg("/nonexistent/file.pdf")
        .output()
        .unwrap();
    assert!(!o.status.success());
}

#[test]
fn help_and_version() {
    let o = Command::new(BIN).arg("--version").output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("pdfdistill-lopdf "));
    let o = Command::new(BIN).arg("--help").output().unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("--max-dpi"));
}
