use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{bail, Context};
use iword::{key, Dictionary, Mode};
use regex::{Regex, RegexBuilder};

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
        if !blocks.is_empty() {
            b = b.add_many(&blocks, key::BLOCK);
        }
        if !alerts.is_empty() {
            b = b.add_many(&alerts, key::ALERT);
        }
        if !flags.is_empty() {
            b = b.add_many(&flags, key::FLAG);
        }
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

    fn engine_name(&self) -> &'static str {
        "iword-rs"
    }
}

// ── Aho-Corasick engine ───────────────────────────────────────────────────────

struct AcScanner {
    block: Option<AhoCorasick>,
    block_words: Vec<String>,
    block_regex: Vec<RegexRule>,
    alert: Option<AhoCorasick>,
    alert_words: Vec<String>,
    alert_regex: Vec<RegexRule>,
    flag: Option<AhoCorasick>,
    flag_words: Vec<String>,
    flag_regex: Vec<RegexRule>,
}

struct RegexRule {
    pattern: String,
    regex: Regex,
}

#[derive(Default)]
struct RuleBuckets {
    block_literals: Vec<String>,
    alert_literals: Vec<String>,
    flag_literals: Vec<String>,
    block_regex: Vec<RegexRule>,
    alert_regex: Vec<RegexRule>,
    flag_regex: Vec<RegexRule>,
}

impl AcScanner {
    fn build(cfg: &KeywordConfig) -> anyhow::Result<Self> {
        let mut rules = RuleBuckets {
            block_literals: cfg.inline_block.iter().map(|s| normalize(s)).collect(),
            alert_literals: cfg.inline_alert.iter().map(|s| normalize(s)).collect(),
            flag_literals: cfg.inline_flag.iter().map(|s| normalize(s)).collect(),
            ..RuleBuckets::default()
        };

        for path in &cfg.dict_paths {
            load_ac_dict_file(path, &mut rules)
                .with_context(|| format!("loading dictionary file {path}"))?;
        }

        let build = |pats: &[String]| -> anyhow::Result<Option<AhoCorasick>> {
            if pats.is_empty() {
                return Ok(None);
            }
            Ok(Some(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::LeftmostFirst)
                    .build(pats)?,
            ))
        };
        Ok(Self {
            block: build(&rules.block_literals)?,
            block_words: rules.block_literals,
            block_regex: rules.block_regex,
            alert: build(&rules.alert_literals)?,
            alert_words: rules.alert_literals,
            alert_regex: rules.alert_regex,
            flag: build(&rules.flag_literals)?,
            flag_words: rules.flag_literals,
            flag_regex: rules.flag_regex,
        })
    }
}

impl InputScanner for AcScanner {
    fn check(&self, norm: &str) -> InputVerdict {
        if let Some(ac) = &self.block {
            if let Some(m) = ac.find(norm) {
                return InputVerdict::Blocked(self.block_words[m.pattern()].clone());
            }
        }
        if let Some(rule) = self.block_regex.iter().find(|r| r.regex.is_match(norm)) {
            return InputVerdict::Blocked(rule.pattern.clone());
        }

        if let Some(ac) = &self.alert {
            if let Some(m) = ac.find(norm) {
                return InputVerdict::Alert(self.alert_words[m.pattern()].clone());
            }
        }
        if let Some(rule) = self.alert_regex.iter().find(|r| r.regex.is_match(norm)) {
            return InputVerdict::Alert(rule.pattern.clone());
        }

        if let Some(ac) = &self.flag {
            if let Some(m) = ac.find(norm) {
                return InputVerdict::Flagged(self.flag_words[m.pattern()].clone());
            }
        }
        if let Some(rule) = self.flag_regex.iter().find(|r| r.regex.is_match(norm)) {
            return InputVerdict::Flagged(rule.pattern.clone());
        }

        InputVerdict::Clean
    }

    fn engine_name(&self) -> &'static str {
        "aho-corasick"
    }
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
            "iword-rs" => Box::new(IwordScanner::build(cfg)?),
            other => {
                bail!("unknown keyword engine `{other}` (expected `iword-rs` or `aho-corasick`)")
            }
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

fn load_ac_dict_file(path: &str, rules: &mut RuleBuckets) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut cols = line.split('\t').map(str::trim);
        let Some(pattern) = cols.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        let key = cols
            .next()
            .ok_or_else(|| anyhow::anyhow!("{path}:{}: missing key", idx + 1))?;
        let key = key
            .parse::<u32>()
            .with_context(|| format!("{path}:{}: invalid key `{key}`", idx + 1))?;

        let target = match key {
            0 => RuleTarget::Block,
            1 => RuleTarget::Alert,
            2 => RuleTarget::Flag,
            other => bail!("{path}:{}: unsupported key `{other}`", idx + 1),
        };

        if let Some(regex_pattern) = parse_regex_pattern(pattern) {
            let regex = RegexBuilder::new(regex_pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("{path}:{}: invalid regex `{regex_pattern}`", idx + 1))?;
            let rule = RegexRule {
                pattern: regex_pattern.to_string(),
                regex,
            };
            match target {
                RuleTarget::Block => rules.block_regex.push(rule),
                RuleTarget::Alert => rules.alert_regex.push(rule),
                RuleTarget::Flag => rules.flag_regex.push(rule),
            }
        } else {
            let literal = normalize(pattern);
            match target {
                RuleTarget::Block => rules.block_literals.push(literal),
                RuleTarget::Alert => rules.alert_literals.push(literal),
                RuleTarget::Flag => rules.flag_literals.push(literal),
            }
        }
    }
    Ok(())
}

enum RuleTarget {
    Block,
    Alert,
    Flag,
}

fn parse_regex_pattern(pattern: &str) -> Option<&str> {
    pattern
        .strip_prefix('/')
        .and_then(|p| p.strip_suffix('/'))
        .filter(|p| !p.is_empty())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
