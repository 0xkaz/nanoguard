//! nanoguard-eval — recognizer evaluation harness.
//!
//! Runs the request-side `Redactor` over a labeled corpus and reports
//! Precision / Recall / F1, broken down per entity. Useful for tuning a
//! dictionary pack against a held-out test set, or for catching
//! regressions in CI.
//!
//! Corpus format: JSONL, one record per line, shaped like:
//!
//!   {"id": "001",
//!    "text": "Contact alice@example.com",
//!    "annotations": [{"type": "EMAIL", "start": 8, "end": 25}]}
//!
//! `text` and `annotations` are required; `id` is optional but
//! recommended for diagnostics.
//!
//! Usage:
//!   nanoguard-eval --gold corpus.jsonl
//!   nanoguard-eval --gold corpus.jsonl --dict dicts/policies/finance.txt
//!   nanoguard-eval --gold corpus.jsonl --json report.json
//!   nanoguard-eval --gold corpus.jsonl --match strict | lenient
//!
//! See _RECOGNIZER_EVAL.md for the full design rationale.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use nanoguard::proxy::redact::{default_inline_patterns, PlaceholderStyle, RedactMatch, Redactor};

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    id: Option<String>,
    text: String,
    #[serde(default)]
    annotations: Vec<Annotation>,
}

#[derive(Debug, Deserialize, Clone)]
struct Annotation {
    #[serde(rename = "type")]
    entity: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    /// Exact start/end + entity match.
    Strict,
    /// Same entity + at least one byte of overlap.
    Lenient,
}

impl MatchMode {
    fn parse_name(s: &str) -> Self {
        match s {
            "strict" => Self::Strict,
            _ => Self::Lenient,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct EntityCounts {
    tp: u64,
    fp: u64,
    fn_count: u64,
}

impl EntityCounts {
    fn precision(&self) -> f64 {
        let denom = self.tp + self.fp;
        if denom == 0 {
            0.0
        } else {
            self.tp as f64 / denom as f64
        }
    }
    fn recall(&self) -> f64 {
        let denom = self.tp + self.fn_count;
        if denom == 0 {
            0.0
        } else {
            self.tp as f64 / denom as f64
        }
    }
    fn f1(&self) -> f64 {
        let p = self.precision();
        let r = self.recall();
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    version: String,
    corpus_path: String,
    record_count: usize,
    annotation_count: usize,
    rule_count: usize,
    match_mode: String,
    totals: EntityCounts,
    by_entity: BTreeMap<String, EntityCounts>,
    false_positive_examples: Vec<MatchExample>,
    false_negative_examples: Vec<MissExample>,
}

#[derive(Debug, Serialize)]
struct MatchExample {
    id: Option<String>,
    text: String,
    matched_entity: String,
    match_text: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Serialize)]
struct MissExample {
    id: Option<String>,
    text: String,
    missed_entity: String,
    expected_text: String,
    start: usize,
    end: usize,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut iter = env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--gold" => {
                args.gold = iter
                    .next()
                    .ok_or_else(|| anyhow!("--gold requires a path"))?
                    .into();
            }
            "--dict" => {
                args.dicts.push(
                    iter.next()
                        .ok_or_else(|| anyhow!("--dict requires a path"))?,
                );
            }
            "--json" => {
                args.json_out = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--json requires a path"))?
                        .into(),
                );
            }
            "--match" => {
                args.match_mode = MatchMode::parse_name(
                    &iter
                        .next()
                        .ok_or_else(|| anyhow!("--match requires strict|lenient"))?,
                );
            }
            "--max-examples" => {
                args.max_examples = iter
                    .next()
                    .ok_or_else(|| anyhow!("--max-examples requires a number"))?
                    .parse()
                    .context("--max-examples is not a number")?;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    if args.gold.as_os_str().is_empty() {
        return Err(anyhow!("--gold <path> is required"));
    }
    Ok(args)
}

fn print_help() {
    eprintln!(
        "nanoguard-eval — recognizer evaluation harness\n\n\
         Usage:\n  \
           nanoguard-eval --gold <corpus.jsonl> [--dict <path>]... [--json <path>] [--match strict|lenient] [--max-examples N]\n\n\
         Options:\n  \
           --gold <path>          JSONL corpus file (required)\n  \
           --dict <path>          Optional extra recognizer dict (repeatable)\n  \
           --json <path>          Write a structured report to this path\n  \
           --match strict|lenient Match mode (default: lenient)\n  \
           --max-examples N       Max FP/FN examples to surface (default: 5)\n"
    );
}

#[derive(Debug)]
struct Args {
    gold: PathBuf,
    dicts: Vec<String>,
    json_out: Option<PathBuf>,
    match_mode: MatchMode,
    max_examples: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            gold: PathBuf::new(),
            dicts: Vec::new(),
            json_out: None,
            match_mode: MatchMode::Lenient,
            max_examples: 5,
        }
    }
}

fn load_corpus(path: &PathBuf) -> Result<Vec<Record>> {
    let f = File::open(path).with_context(|| format!("opening corpus {}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading corpus line {}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: Record =
            serde_json::from_str(&line).with_context(|| format!("parsing JSONL line {}", i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

fn matches_overlap(detected: &RedactMatch, gold: &Annotation, mode: MatchMode) -> bool {
    if detected.entity != gold.entity {
        return false;
    }
    match mode {
        MatchMode::Strict => detected.start == gold.start && detected.end == gold.end,
        MatchMode::Lenient => detected.start < gold.end && gold.start < detected.end,
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let corpus = load_corpus(&args.gold)?;
    let redactor = Redactor::build_with_style(
        &default_inline_patterns(),
        &args.dicts,
        PlaceholderStyle::Bare,
    )?;
    let rule_count = redactor.rule_count();

    let mut by_entity: BTreeMap<String, EntityCounts> = BTreeMap::new();
    let mut totals = EntityCounts::default();
    let mut fps: Vec<MatchExample> = Vec::new();
    let mut fns: Vec<MissExample> = Vec::new();
    let mut annotation_count: usize = 0;

    for rec in &corpus {
        annotation_count += rec.annotations.len();
        let detected = redactor.find_matches(&rec.text);

        // Track which gold annotations were hit and which detections were
        // not justified by any gold annotation.
        let mut detected_matched: Vec<bool> = vec![false; detected.len()];
        let mut gold_matched: Vec<bool> = vec![false; rec.annotations.len()];

        for (di, d) in detected.iter().enumerate() {
            for (gi, g) in rec.annotations.iter().enumerate() {
                if !gold_matched[gi] && matches_overlap(d, g, args.match_mode) {
                    detected_matched[di] = true;
                    gold_matched[gi] = true;
                    by_entity.entry(g.entity.clone()).or_default().tp += 1;
                    totals.tp += 1;
                    break;
                }
            }
        }
        for (di, d) in detected.iter().enumerate() {
            if !detected_matched[di] {
                by_entity.entry(d.entity.clone()).or_default().fp += 1;
                totals.fp += 1;
                if fps.len() < args.max_examples {
                    fps.push(MatchExample {
                        id: rec.id.clone(),
                        text: rec.text.clone(),
                        matched_entity: d.entity.clone(),
                        match_text: d.matched_text.clone(),
                        start: d.start,
                        end: d.end,
                    });
                }
            }
        }
        for (gi, g) in rec.annotations.iter().enumerate() {
            if !gold_matched[gi] {
                by_entity.entry(g.entity.clone()).or_default().fn_count += 1;
                totals.fn_count += 1;
                if fns.len() < args.max_examples {
                    let expected_text = rec.text.get(g.start..g.end).unwrap_or("?").to_string();
                    fns.push(MissExample {
                        id: rec.id.clone(),
                        text: rec.text.clone(),
                        missed_entity: g.entity.clone(),
                        expected_text,
                        start: g.start,
                        end: g.end,
                    });
                }
            }
        }
    }

    let report = Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        corpus_path: args.gold.display().to_string(),
        record_count: corpus.len(),
        annotation_count,
        rule_count,
        match_mode: format!("{:?}", args.match_mode).to_lowercase(),
        totals,
        by_entity,
        false_positive_examples: fps,
        false_negative_examples: fns,
    };

    print_report(&report);

    if let Some(out) = &args.json_out {
        let serialized = serde_json::to_string_pretty(&report)?;
        let mut f =
            File::create(out).with_context(|| format!("creating report at {}", out.display()))?;
        f.write_all(serialized.as_bytes())?;
        eprintln!("\nJSON report written to {}", out.display());
    }

    // Exit non-zero when at least one entity has F1=0 with annotations
    // present (signal for CI gating). Otherwise exit 0.
    let any_zero = report
        .by_entity
        .values()
        .any(|c| (c.tp + c.fn_count) > 0 && c.f1() == 0.0);
    std::process::exit(if any_zero { 2 } else { 0 });
}

fn print_report(r: &Report) {
    println!("nanoguard-eval v{}", r.version);
    println!(
        "Corpus: {} ({} record(s), {} annotation(s))",
        r.corpus_path, r.record_count, r.annotation_count
    );
    println!("Rules:  {} entity rules", r.rule_count);
    println!("Match:  {}", r.match_mode);
    println!();
    println!(
        "{:<28} {:>6} {:>6} {:>6} {:>10} {:>8} {:>8}",
        "Entity", "TP", "FP", "FN", "Precision", "Recall", "F1"
    );
    println!("{}", "─".repeat(80));
    let mut all_entities: BTreeSet<String> = r.by_entity.keys().cloned().collect();
    if all_entities.is_empty() {
        all_entities.insert("(none)".to_string());
    }
    for entity in &all_entities {
        if let Some(c) = r.by_entity.get(entity) {
            println!(
                "{:<28} {:>6} {:>6} {:>6} {:>10.3} {:>8.3} {:>8.3}",
                entity,
                c.tp,
                c.fp,
                c.fn_count,
                c.precision(),
                c.recall(),
                c.f1()
            );
        }
    }
    println!("{}", "─".repeat(80));
    println!(
        "{:<28} {:>6} {:>6} {:>6} {:>10.3} {:>8.3} {:>8.3}",
        "TOTAL",
        r.totals.tp,
        r.totals.fp,
        r.totals.fn_count,
        r.totals.precision(),
        r.totals.recall(),
        r.totals.f1()
    );

    if !r.false_positive_examples.is_empty() {
        println!("\nTop false positives:");
        for ex in &r.false_positive_examples {
            let id = ex.id.as_deref().unwrap_or("?");
            println!(
                "  [{id}] {:?} matched as {} → \"{}\"",
                truncate(&ex.text, 80),
                ex.matched_entity,
                ex.match_text
            );
        }
    }
    if !r.false_negative_examples.is_empty() {
        println!("\nTop false negatives:");
        for ex in &r.false_negative_examples {
            let id = ex.id.as_deref().unwrap_or("?");
            println!(
                "  [{id}] missed {} ({}..{}): {:?}",
                ex.missed_entity, ex.start, ex.end, ex.expected_text
            );
        }
    }

    // Suppress unused-import warning when the file compiles standalone.
    let _ = HashMap::<&str, &str>::new();
    let _ = serde_json::json!({});
    let _ = Value::Null;
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
