use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use iword::{key, Dictionary, Mode};

use crate::config::KeywordConfig;

#[derive(Debug, PartialEq)]
pub enum InputVerdict {
    Blocked(String),
    Alert(String),
    Flagged(String),
    Clean,
}

// ── Text normalisation (shared by both engines) ───────────────────────────────

fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = true;
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' || ch == '\t' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else if ch.is_uppercase() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if ch == ' ' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// ── Input scanner trait ───────────────────────────────────────────────────────

pub trait InputScanner: Send + Sync {
    fn check(&self, normalized: &str) -> InputVerdict;
    fn engine_name(&self) -> &'static str;
}

// ── iword-rs engine ───────────────────────────────────────────────────────────

struct IwordScanner {
    dict: Dictionary,
}

impl IwordScanner {
    fn build(cfg: &KeywordConfig) -> anyhow::Result<Self> {
        let mut b = Dictionary::builder();
        let blocks: Vec<&str> = cfg.inline_block.iter().map(String::as_str).collect();
        let alerts: Vec<&str> = cfg.inline_alert.iter().map(String::as_str).collect();
        let flags: Vec<&str> = cfg.inline_flag.iter().map(String::as_str).collect();
        if !blocks.is_empty() { b = b.add_many(&blocks, key::BLOCK); }
        if !alerts.is_empty() { b = b.add_many(&alerts, key::ALERT); }
        if !flags.is_empty()  { b = b.add_many(&flags,  key::FLAG);  }
        for path in &cfg.dict_paths {
            b = b.load_file(path).map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(Self { dict: b.build() })
    }
}

impl InputScanner for IwordScanner {
    fn check(&self, norm: &str) -> InputVerdict {
        let mode = Mode::FORBID;
        if let Some(m) = self.dict.scan_key(norm, key::BLOCK, mode).first() {
            return InputVerdict::Blocked(m.extract(norm).to_string());
        }
        if let Some(m) = self.dict.scan_key(norm, key::ALERT, mode).first() {
            return InputVerdict::Alert(m.extract(norm).to_string());
        }
        if let Some(r) = self.dict.classify(norm, mode) {
            if r.key == key::FLAG {
                return InputVerdict::Flagged(format!("score={:.1}", r.score));
            }
        }
        InputVerdict::Clean
    }

    fn engine_name(&self) -> &'static str { "iword-rs" }
}

// ── Aho-Corasick engine ───────────────────────────────────────────────────────

struct AcScanner {
    block: AhoCorasick,
    block_words: Vec<String>,
    alert: AhoCorasick,
    alert_words: Vec<String>,
    flag: AhoCorasick,
    flag_words: Vec<String>,
}

impl AcScanner {
    fn build(cfg: &KeywordConfig) -> anyhow::Result<Self> {
        let build = |pats: &[String]| -> anyhow::Result<AhoCorasick> {
            Ok(AhoCorasickBuilder::new()
                .match_kind(MatchKind::LeftmostFirst)
                .build(pats)?)
        };
        Ok(Self {
            block: build(&cfg.inline_block)?,
            block_words: cfg.inline_block.clone(),
            alert: build(&cfg.inline_alert)?,
            alert_words: cfg.inline_alert.clone(),
            flag: build(&cfg.inline_flag)?,
            flag_words: cfg.inline_flag.clone(),
        })
    }
}

impl InputScanner for AcScanner {
    fn check(&self, norm: &str) -> InputVerdict {
        if let Some(m) = self.block.find(norm) {
            return InputVerdict::Blocked(self.block_words[m.pattern()].clone());
        }
        if let Some(m) = self.alert.find(norm) {
            return InputVerdict::Alert(self.alert_words[m.pattern()].clone());
        }
        if self.flag.find(norm).is_some() {
            if let Some(m) = self.flag.find(norm) {
                return InputVerdict::Flagged(self.flag_words[m.pattern()].clone());
            }
        }
        InputVerdict::Clean
    }

    fn engine_name(&self) -> &'static str { "aho-corasick" }
}

// ── Public Matchers struct ────────────────────────────────────────────────────

pub struct Matchers {
    input: Box<dyn InputScanner>,
    output: Dictionary,
}

impl Matchers {
    pub fn build(cfg: &KeywordConfig) -> anyhow::Result<Self> {
        Self::build_with_engine(cfg, &cfg.engine)
    }

    pub fn build_with_engine(cfg: &KeywordConfig, engine: &str) -> anyhow::Result<Self> {
        let input: Box<dyn InputScanner> = match engine {
            "aho-corasick" => Box::new(AcScanner::build(cfg)?),
            _ => Box::new(IwordScanner::build(cfg)?),
        };

        let output = Dictionary::builder()
            .add_many(&["ssn", "social security", "credit card"], key::BLOCK)
            .add_many(&["I cannot", "As an AI", "I'm not able"], key::FLAG)
            .build();

        Ok(Self { input, output })
    }

    pub fn engine_name(&self) -> &'static str {
        self.input.engine_name()
    }

    pub fn check_input(&self, text: &str) -> InputVerdict {
        self.input.check(&normalize(text))
    }

    pub fn filter_output(&self, text: &str) -> String {
        self.output.filter(text, Mode::FORBID)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
