use crate::config::KeywordConfig;
use iword::{key, Dictionary, Mode};

pub struct Matchers {
    pub input: Dictionary,
    pub output: Dictionary,
}

#[derive(Debug, PartialEq)]
pub enum InputVerdict {
    Blocked(String),
    Alert(String),
    Flagged(String),
    Clean,
}

impl Matchers {
    pub fn build(cfg: &KeywordConfig) -> anyhow::Result<Self> {
        let mut builder = Dictionary::builder();

        // Inline entries from config
        let block_refs: Vec<&str> = cfg.inline_block.iter().map(String::as_str).collect();
        let alert_refs: Vec<&str> = cfg.inline_alert.iter().map(String::as_str).collect();
        let flag_refs: Vec<&str> = cfg.inline_flag.iter().map(String::as_str).collect();

        if !block_refs.is_empty() {
            builder = builder.add_many(&block_refs, key::BLOCK);
        }
        if !alert_refs.is_empty() {
            builder = builder.add_many(&alert_refs, key::ALERT);
        }
        if !flag_refs.is_empty() {
            builder = builder.add_many(&flag_refs, key::FLAG);
        }

        // Additional dict files
        for path in &cfg.dict_paths {
            builder = builder.load_file(path).map_err(|e| anyhow::anyhow!(e))?;
        }

        let input = builder.build();

        let output = Dictionary::builder()
            .add_many(&["ssn", "social security", "credit card"], key::BLOCK)
            .add_many(&["I cannot", "As an AI", "I'm not able"], key::FLAG)
            .build();

        Ok(Self { input, output })
    }

    pub fn check_input(&self, text: &str) -> InputVerdict {
        let mode = Mode::FORBID;
        let lower = text.to_lowercase().replace(['\n', '\r'], " ");
        let lower = lower.split_whitespace().collect::<Vec<_>>().join(" ");

        if let Some(m) = self.input.scan_key(&lower, key::BLOCK, mode).first() {
            return InputVerdict::Blocked(m.extract(lower.as_str()).to_string());
        }
        if let Some(m) = self.input.scan_key(&lower, key::ALERT, mode).first() {
            return InputVerdict::Alert(m.extract(lower.as_str()).to_string());
        }
        if let Some(r) = self.input.classify(&lower, mode) {
            if r.key == key::FLAG {
                return InputVerdict::Flagged(format!("score={:.1}", r.score));
            }
        }
        InputVerdict::Clean
    }

    pub fn filter_output(&self, text: &str) -> String {
        self.output.filter(text, Mode::FORBID)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
