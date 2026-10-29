//! pdfdistill: shrink bloated PDFs using lopdf.
//!
//! Pipeline: dedup -> font subsetting -> image recompression/downsampling -> structural
//! compaction (Flate level 9, pruning, object + xref streams).

pub mod content;
pub mod dedup;
pub mod fonts;
pub mod images;
pub mod structure;
pub mod util;

use lopdf::Document;

#[derive(Clone, Debug)]
pub struct Options {
    pub quality: u8,
    pub max_dpi: u32,
    pub images: bool,
    pub subset: bool,
    pub dedup: bool,
    pub lossless: bool,
    /// Measure the serialized size after each step (slower).
    pub measure_steps: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            quality: 75,
            max_dpi: 150,
            images: true,
            subset: true,
            dedup: true,
            lossless: false,
            measure_steps: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub input_size: usize,
    pub output_size: usize,
    /// True if the optimized file was not smaller, so the input was returned unchanged.
    pub kept_original: bool,
    /// Human-readable per-step lines.
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn pct(before: usize, after: usize) -> String {
    if before == 0 {
        return "0.0%".into();
    }
    format!(
        "{:+.1}%",
        (after as f64 - before as f64) * 100.0 / before as f64
    )
}

/// Optimize a PDF held in memory. The returned bytes are never larger than `input`.
pub fn distill(input: &[u8], opts: &Options) -> Result<(Vec<u8>, Report), Error> {
    let mut report = Report {
        input_size: input.len(),
        ..Default::default()
    };
    let mut doc = Document::load_mem(input).map_err(|e| Error(format!("cannot parse PDF: {e}")))?;
    if doc.is_encrypted() || doc.was_encrypted() || doc.trailer.has(b"Encrypt") {
        report.warnings.push("encrypted PDF: left unchanged".into());
        report.kept_original = true;
        report.output_size = input.len();
        return Ok((input.to_vec(), report));
    }
    // Objects that lived in object streams are now in `objects`; the containers are rebuilt.
    doc.objects
        .retain(|_, o| !matches!(o.type_name(), Ok(b"ObjStm") | Ok(b"XRef")));

    let mut warn = |w: String| report.warnings.push(w);
    let mut steps: Vec<String> = Vec::new();
    let mut last = input.len();
    let mut measure = |doc: &mut Document, label: &str, detail: String, steps: &mut Vec<String>| {
        if opts.measure_steps {
            let size = util::serialize(doc).map(|v| v.len()).unwrap_or(0);
            steps.push(format!(
                "{label:<10} {size:>10} bytes ({}){}",
                pct(last, size),
                detail
            ));
            last = size;
        } else {
            steps.push(format!("{label:<10}{detail}"));
        }
    };

    if opts.dedup {
        let s = dedup::dedup(&mut doc);
        util::prune_unreferenced(&mut doc);
        measure(
            &mut doc,
            "dedup",
            format!("  merged {} objects", s.merged),
            &mut steps,
        );
    }

    let need_usage = (opts.subset) || (opts.images && !opts.lossless);
    let usage = if need_usage {
        Some(content::analyse(&doc))
    } else {
        None
    };

    if opts.subset {
        let s = fonts::subset_fonts(&mut doc, usage.as_ref().unwrap(), &mut warn);
        measure(
            &mut doc,
            "fonts",
            format!(
                "  subset {} font(s), skipped {}; font programs {} -> {} bytes",
                s.subset, s.skipped, s.bytes_before, s.bytes_after
            ),
            &mut steps,
        );
    }

    if opts.images && !opts.lossless {
        let io = images::ImageOptions {
            quality: opts.quality,
            max_dpi: opts.max_dpi,
            jpeg: true,
        };
        let s = images::optimize_images(&mut doc, usage.as_ref().unwrap(), &io, &mut warn);
        measure(
            &mut doc,
            "images",
            format!(
                "  {} to JPEG, {} re-deflated, {} downsampled; {} -> {} bytes",
                s.to_jpeg, s.reflated, s.downsampled, s.bytes_before, s.bytes_after
            ),
            &mut steps,
        );
    }

    let s = structure::optimize_structure(&mut doc);
    let out = util::serialize(&mut doc).map_err(|e| Error(format!("cannot write PDF: {e}")))?;
    steps.push(format!(
        "{:<10} {:>10} bytes ({})  recompressed {} streams, pruned {} objects",
        "structure",
        out.len(),
        pct(last, out.len()),
        s.recompressed,
        s.pruned
    ));
    report.steps = steps;

    // Sanity check: the result must parse.
    if let Err(e) = Document::load_mem(&out) {
        report.warnings.push(format!(
            "optimized output failed to re-parse ({e}); keeping original"
        ));
        report.kept_original = true;
    } else if out.len() >= input.len() {
        report.kept_original = true;
    }
    if report.kept_original {
        report.output_size = input.len();
        return Ok((input.to_vec(), report));
    }
    report.output_size = out.len();
    Ok((out, report))
}
