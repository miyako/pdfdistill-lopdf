use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pdfdistill::{distill, Options};

const NAME: &str = "pdfdistill-lopdf";

const HELP: &str = "\
Reduce the size of bloated PDFs (e.g. from Microsoft Print to PDF).

Usage: pdfdistill-lopdf <INPUT|-> [-o <OUTPUT|->] [options]

Arguments:
  INPUT                  input PDF path, or '-' for stdin

Options:
  -o <OUTPUT|->          output path or '-' for stdout
                         (default: stdout if INPUT is '-', else <input>.distilled.pdf)
  --quality <1-100>      JPEG quality for recompressed images (default 75)
  --max-dpi <n>          downsample images whose effective resolution exceeds n
                         (default 150; 0 = never)
  --no-images            skip image recompression
  --no-subset            skip font subsetting
  --no-dedup             skip duplicate object/image/font removal
  --lossless             only lossless optimizations (no JPEG recompression / downsampling)
  -v, --verbose          print per-step size statistics to stderr
  -q, --quiet            print nothing but errors
  --version              print version and exit
  -h, --help             print this help and exit
";

struct Cli {
    input: String,
    output: Option<String>,
    opts: Options,
    verbose: bool,
    quiet: bool,
}

fn parse_args(args: Vec<String>) -> Result<Option<Cli>, String> {
    let mut input = None;
    let mut output = None;
    let mut opts = Options::default();
    let (mut verbose, mut quiet) = (false, false);
    let mut it = args.into_iter();
    let mut positional_only = false;
    while let Some(a) = it.next() {
        if positional_only || a == "-" || !a.starts_with('-') {
            if input.replace(a).is_some() {
                return Err("only one INPUT may be given".into());
            }
            continue;
        }
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match flag.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-o" | "--output" => output = Some(value("-o")?),
            "--quality" => {
                let v = value("--quality")?;
                opts.quality = match v.parse::<u8>() {
                    Ok(q) if (1..=100).contains(&q) => q,
                    _ => {
                        return Err(format!(
                            "--quality must be an integer between 1 and 100 (got '{v}')"
                        ))
                    }
                };
            }
            "--max-dpi" => {
                let v = value("--max-dpi")?;
                opts.max_dpi = v
                    .parse::<u32>()
                    .map_err(|_| format!("--max-dpi must be a non-negative integer (got '{v}')"))?;
            }
            "--no-images" => opts.images = false,
            "--no-subset" => opts.subset = false,
            "--no-dedup" => opts.dedup = false,
            "--lossless" => opts.lossless = true,
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            _ => return Err(format!("unknown option '{a}'")),
        }
    }
    let input = input.ok_or("missing INPUT (use '-' for stdin)")?;
    opts.measure_steps = verbose && !quiet;
    Ok(Some(Cli {
        input,
        output,
        opts,
        verbose: verbose && !quiet,
        quiet,
    }))
}

/// `foo.pdf` -> `foo.distilled.pdf`; other names get `.distilled.pdf` appended.
fn default_output(input: &str) -> PathBuf {
    let p = Path::new(input);
    let is_pdf = p
        .extension()
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false);
    let base = if is_pdf {
        p.with_extension("")
    } else {
        p.to_path_buf()
    };
    let mut s = base.into_os_string();
    s.push(".distilled.pdf");
    PathBuf::from(s)
}

fn run() -> Result<(), String> {
    let Some(cli) = parse_args(std::env::args().skip(1).collect())? else {
        return Ok(());
    };

    let input = if cli.input == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        buf
    } else {
        std::fs::read(&cli.input).map_err(|e| format!("reading '{}': {e}", cli.input))?
    };

    let start = std::time::Instant::now();
    let (out, report) = distill(&input, &cli.opts).map_err(|e| e.to_string())?;

    let target = match cli.output.as_deref() {
        Some("-") => None,
        Some(p) => Some(PathBuf::from(p)),
        None if cli.input == "-" => None,
        None => Some(default_output(&cli.input)),
    };
    match &target {
        None => {
            let mut so = std::io::stdout().lock();
            so.write_all(&out)
                .and_then(|_| so.flush())
                .map_err(|e| format!("writing stdout: {e}"))?;
        }
        Some(p) => {
            std::fs::write(p, &out).map_err(|e| format!("writing '{}': {e}", p.display()))?
        }
    }

    if !cli.quiet {
        for w in &report.warnings {
            eprintln!("{NAME}: warning: {w}");
        }
        if cli.verbose {
            eprintln!("{:<10} {:>10} bytes", "input", report.input_size);
            for s in &report.steps {
                eprintln!("{s}");
            }
        }
        let saved = 100.0 * (1.0 - report.output_size as f64 / report.input_size.max(1) as f64);
        let dest = target
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<stdout>".into());
        eprintln!(
            "{NAME}: {} -> {} bytes ({saved:.1}% smaller{}) -> {dest} [{:.2}s]",
            report.input_size,
            report.output_size,
            if report.kept_original {
                ", original kept"
            } else {
                ""
            },
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_names() {
        assert_eq!(
            default_output("a/b.pdf"),
            PathBuf::from("a/b.distilled.pdf")
        );
        assert_eq!(default_output("x.PDF"), PathBuf::from("x.distilled.pdf"));
        assert_eq!(
            default_output("noext"),
            PathBuf::from("noext.distilled.pdf")
        );
    }

    #[test]
    fn rejects_bad_quality() {
        let r = parse_args(vec!["in.pdf".into(), "--quality".into(), "0".into()]);
        assert!(r.is_err());
    }
}
