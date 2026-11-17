use std::sync::LazyLock;

use anyhow::{Result, ensure};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

use super::policy::{Policy, Risk, RuleKind, Strategy};

pub const BLOCK_TEXT: &str =
    "SECURITY_RESPONSE_BLOCKED：回傳含敏感資訊或無法完整檢查；請在本機安全中心檢視事件。";
const MAX_CHARS: usize = 4 * 1024 * 1024;
const MAX_MATCHES: usize = 10000;
const MAX_NODES: usize = 50000;
const MAX_DEPTH: usize = 32;

static PRIVATE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----END (?:[A-Z0-9 ]{0,30})PRIVATE KEY-----").unwrap());

fn builtin(name: &str) -> &str {
    match name {
        "private_key" => r"-----BEGIN (?:[A-Z0-9 ]{0,30})PRIVATE KEY-----",
        "api_key" => {
            r"(?i)(?:\b(?:sk|ghp|g[iİı]thub_pat|xox[baprs])[-_][a-z0-9İıſK_-]{8,}|\bAK[iİı]A[A-Z0-9İıſK]{16}\b)"
        }
        "bearer" => r"(?i)\bBearer\s+[a-z0-9İıſK._~+/=-]{8,}",
        "password" => {
            r#"(?i)["']?(?:password|passwd|secret|ap[iİı][_-]?key|access[_-]?token)["']?\s*[:=]\s*(?:"[^"\r\n]{3,}"|'[^'\r\n]{3,}'|[^\s,"';}\]]{3,})"#
        }
        "email" => r"(?i)\b[a-z0-9İıſK._%+-]{1,64}@[a-z0-9İıſK.-]{1,190}\.[a-zİıſK]{2,20}\b",
        _ => unreachable!("validated policy"),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub rule: String,
    pub risk: Risk,
    pub count: usize,
}

struct Rule {
    id: String,
    risk: Risk,
    regex: Regex,
    private_key: bool,
    password: bool,
}

pub enum Review {
    Passed(Value),
    Masked {
        payload: Value,
        findings: Vec<Finding>,
    },
    Blocked {
        findings: Vec<Finding>,
        checked: bool,
    },
}

pub struct Guard {
    strategy: Strategy,
    rules: Vec<Rule>,
}

impl Guard {
    pub fn new(policy: &Policy) -> Result<Self> {
        policy.validate()?;
        let mut rules = Vec::new();
        for rule in policy
            .rules
            .iter()
            .filter(|rule| rule.enabled && policy.detector_enabled)
        {
            let builtin_rule = rule.kind == RuleKind::Builtin;
            let pattern = if builtin_rule {
                builtin(&rule.pattern).to_owned()
            } else {
                regex::escape(&rule.pattern)
            };
            rules.push(Rule {
                id: rule.id.clone(),
                risk: rule.risk,
                regex: Regex::new(&pattern)
                    .map_err(|_| anyhow::anyhow!("SECURITY_RULE_COMPILE"))?,
                private_key: builtin_rule && rule.pattern == "private_key",
                password: builtin_rule && rule.pattern == "password",
            });
        }
        Ok(Self {
            strategy: policy.strategy,
            rules,
        })
    }

    pub fn inspect(&self, payload: Value) -> Review {
        let mut scan = Scan {
            guard: self,
            findings: Vec::new(),
            chars: 0,
            nodes: 0,
            matches: 0,
        };
        match scan.visit(payload, 0) {
            Err(_) => Review::Blocked {
                findings: Vec::new(),
                checked: false,
            },
            Ok(payload) if scan.findings.is_empty() => Review::Passed(payload),
            Ok(_) if self.strategy == Strategy::Block => Review::Blocked {
                findings: scan.findings,
                checked: true,
            },
            Ok(payload) => Review::Masked {
                payload,
                findings: scan.findings,
            },
        }
    }
}

struct Scan<'a> {
    guard: &'a Guard,
    findings: Vec<Finding>,
    chars: usize,
    nodes: usize,
    matches: usize,
}

impl Scan<'_> {
    fn account_text(&mut self, text: &str) -> Result<()> {
        self.chars += text.chars().count();
        ensure!(self.chars <= MAX_CHARS, "SECURITY_SCAN_LIMIT");
        Ok(())
    }

    fn record(&mut self, rule: &Rule, count: usize) {
        if count > 0 {
            self.findings.push(Finding {
                rule: rule.id.clone(),
                risk: rule.risk,
                count,
            });
        }
    }

    fn one_match(&mut self) -> Result<()> {
        self.matches += 1;
        ensure!(self.matches <= MAX_MATCHES, "SECURITY_MATCH_LIMIT");
        Ok(())
    }

    fn mask(&mut self, text: &str) -> Result<String> {
        let mut spans = Vec::new();
        for rule in &self.guard.rules {
            let mut count = 0;
            let mut private_until = 0;
            for found in rule.regex.find_iter(text) {
                if found.start() < private_until {
                    continue;
                }
                self.one_match()?;
                let end = if rule.private_key {
                    private_until = PRIVATE_END
                        .find_at(text, found.end())
                        .map_or(text.len(), |end| end.end());
                    private_until
                } else {
                    found.end()
                };
                spans.push((found.start(), end));
                count += 1;
            }
            self.record(rule, count);
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in spans {
            if let Some(previous) = merged.last_mut()
                && start <= previous.1
            {
                previous.1 = previous.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        let mut value = String::with_capacity(text.len());
        let mut position = 0;
        for (start, end) in merged {
            value.push_str(&text[position..start]);
            value.extend(std::iter::repeat_n('█', text[start..end].chars().count()));
            position = end;
        }
        value.push_str(&text[position..]);
        Ok(value)
    }

    fn assignment(&mut self, key: &str, value: &Value) -> Result<bool> {
        let text = match value {
            Value::String(text) => text.as_str().into(),
            Value::Number(value) => std::borrow::Cow::Owned(value.to_string()),
            _ => return Ok(false),
        };
        // Count the value even when contextual masking replaces the whole field.
        ensure!(
            text.chars().count() <= MAX_CHARS.saturating_sub(self.chars),
            "SECURITY_SCAN_LIMIT"
        );
        let mut found = false;
        if self.guard.rules.iter().any(|rule| rule.password) {
            let pair = format!("{key}={text}");
            for rule in self.guard.rules.iter().filter(|rule| rule.password) {
                let mut count = 0;
                for _ in rule
                    .regex
                    .find_iter(&pair)
                    .filter(|matched| matched.start() < key.len() && matched.end() > key.len() + 1)
                {
                    self.one_match()?;
                    count += 1;
                }
                self.record(rule, count);
                found |= count != 0;
            }
        }
        if found {
            self.account_text(&text)?;
        }
        Ok(found)
    }

    fn visit(&mut self, value: Value, depth: usize) -> Result<Value> {
        self.nodes += 1;
        ensure!(
            self.nodes <= MAX_NODES && depth <= MAX_DEPTH,
            "SECURITY_SCAN_LIMIT"
        );
        match value {
            Value::String(text) => {
                self.account_text(&text)?;
                let masked = self.mask(&text)?;
                if text.trim_start().starts_with(['{', '[']) {
                    let inner = match serde_json::from_str::<Value>(&text) {
                        Ok(inner) => inner,
                        Err(error) if error.to_string().starts_with("recursion limit exceeded") => {
                            anyhow::bail!("SECURITY_SCAN_LIMIT");
                        }
                        Err(_) => return Ok(masked.into()),
                    };
                    let safe = self.visit(inner, depth + 1)?;
                    let rendered = serde_json::to_string(&safe)?;
                    // The encoded and decoded forms are both observable.
                    ensure!(
                        !self
                            .guard
                            .rules
                            .iter()
                            .any(|rule| rule.regex.find_iter(&rendered).any(|found| {
                                !rule.password || !found.as_str().ends_with(":\"[已遮罩]\"")
                            })),
                        "SECURITY_ENCODED_MATCH"
                    );
                    return Ok(rendered.into());
                }
                Ok(masked.into())
            }
            Value::Array(items) => Ok(Value::Array(
                items
                    .into_iter()
                    .map(|item| self.visit(item, depth + 1))
                    .collect::<Result<_>>()?,
            )),
            Value::Object(fields) => {
                ensure!(
                    !matches!(
                        fields.get("type").and_then(Value::as_str),
                        Some("image" | "audio" | "resource_link")
                    ) && !fields.contains_key("blob")
                        && !matches!(
                            fields.get("encoding").and_then(Value::as_str),
                            Some("base64" | "hex")
                        ),
                    "SECURITY_UNINSPECTABLE"
                );
                let mut safe = Map::with_capacity(fields.len());
                for (key, item) in fields {
                    let safe_key = match self.visit(Value::String(key.clone()), depth + 1)? {
                        Value::String(value) => value,
                        _ => unreachable!("string guard"),
                    };
                    ensure!(!safe.contains_key(&safe_key), "SECURITY_KEY_COLLISION");
                    let value = if self.assignment(&key, &item)? {
                        "[已遮罩]".into()
                    } else {
                        self.visit(item, depth + 1)?
                    };
                    safe.insert(safe_key, value);
                }
                Ok(Value::Object(safe))
            }
            scalar => Ok(scalar),
        }
    }
}
