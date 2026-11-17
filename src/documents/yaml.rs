use std::{collections::HashSet, str::FromStr, sync::LazyLock};

use anyhow::{Result, bail, ensure};
use chrono::NaiveDate;
use num_bigint::BigInt;
use regex::Regex;
use serde_json::{Map, Number, Value};
use yaml_rust2::{
    parser::{Event, Parser},
    scanner::TScalarStyle,
};

use super::MAX_UNITS;

const INVALID: &str = "DOCUMENT_YAML：YAML 結構無效或超出支援範圍。";
const STRUCTURE: &str = "DOCUMENT_STRUCTURE_LIMIT：YAML 節點、深度或 scalar 超限。";
const NODE_PROPERTIES: &str = "DOCUMENT_YAML：禁止 anchor、alias 與明確 tag。";
const MAX_DEPTH: usize = 64;

static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\A[-+]?(?:0b[01_]+|0[0-7_]+|0|[1-9][0-9_]*|0x[0-9a-fA-F_]+|[1-9][0-9_]*(?::[0-5]?[0-9])+)\z"
).unwrap()
});
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\A(?:[-+]?[0-9][0-9_]*\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\z"
).unwrap()
});
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A([0-9]{4})-([0-9]{2})-([0-9]{2})\z").unwrap());
static DATETIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\A([0-9]{4})-([0-9]{1,2})-([0-9]{1,2})(?:[Tt]|[ \t]+)([0-9]{1,2}):([0-9]{2}):([0-9]{2})(?:\.([0-9]*))?(?:[ \t]*(Z|([+-])([0-9]{1,2})(?::([0-9]{2}))?))?\z"
).unwrap()
});

fn scalar(raw: String, style: TScalarStyle) -> Result<(Value, bool)> {
    ensure!(raw.chars().count() <= 128 * 1024, "{STRUCTURE}");
    if style != TScalarStyle::Plain {
        return Ok((Value::String(raw), false));
    }
    let value = match raw.as_str() {
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => Value::Bool(true),
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
            Value::Bool(false)
        }
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "<<" | "=" => bail!("{INVALID}"),
        _ if INT.is_match(&raw) => Value::Number(integer(&raw)?),
        _ if FLOAT.is_match(&raw) => Value::Number(float(&raw)?),
        _ => {
            if let Some(date) = timestamp(&raw)? {
                return Ok((Value::String(date), true));
            }
            Value::String(raw)
        }
    };
    Ok((value, false))
}

fn integer(raw: &str) -> Result<Number> {
    let text = raw.replace('_', "");
    let magnitude = text.trim_start_matches(['+', '-']);
    let (base, digits) = if let Some(digits) = magnitude.strip_prefix("0b") {
        (2, digits)
    } else if let Some(digits) = magnitude.strip_prefix("0x") {
        (16, digits)
    } else if magnitude.starts_with('0') {
        (8, magnitude)
    } else {
        (10, magnitude)
    };
    let mut value = if digits.contains(':') {
        let mut current = BigInt::from(0);
        for part in digits.split(':') {
            ensure!(part.len() <= 4300, "{STRUCTURE}");
            let part = BigInt::parse_bytes(part.as_bytes(), 10)
                .ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
            current = current * 60 + part;
            ensure!(current.bits() <= 14_285, "{STRUCTURE}");
        }
        current
    } else {
        ensure!(base != 10 || digits.len() <= 4300, "{STRUCTURE}");
        BigInt::parse_bytes(digits.as_bytes(), base).ok_or_else(|| anyhow::anyhow!("{INVALID}"))?
    };
    ensure!(value.bits() <= 14_285, "{STRUCTURE}");
    if text.starts_with('-') {
        value = -value;
    }
    let serialized = value.to_str_radix(10);
    ensure!(
        serialized.trim_start_matches('-').len() <= 4300,
        "{STRUCTURE}"
    );
    Ok(Number::from_str(&serialized)?)
}

fn float(raw: &str) -> Result<Number> {
    let text = raw.replace('_', "");
    let magnitude = text.trim_start_matches(['+', '-']);
    let value = if magnitude.contains(':') {
        let mut value = 0.;
        for part in magnitude.split(':') {
            value = value * 60. + part.parse::<f64>()?;
        }
        if text.starts_with('-') { -value } else { value }
    } else {
        ensure!(
            !matches!(magnitude.to_ascii_lowercase().as_str(), ".nan" | ".inf"),
            "DOCUMENT_NUMBER：非有限數字。"
        );
        text.parse()?
    };
    Number::from_f64(value).ok_or_else(|| anyhow::anyhow!("DOCUMENT_NUMBER：非有限數字。"))
}

fn timestamp(raw: &str) -> Result<Option<String>> {
    let date_only = DATE.is_match(raw);
    let date = if date_only {
        DATE.captures(raw)
    } else {
        DATETIME.captures(raw)
    };
    let Some(values) = date else {
        return Ok(None);
    };
    let year: i32 = values[1].parse()?;
    ensure!((1..=9999).contains(&year), "{INVALID}");
    let day = NaiveDate::from_ymd_opt(year, values[2].parse()?, values[3].parse()?)
        .ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
    if date_only {
        return Ok(Some(day.to_string()));
    }
    let fraction = values.get(7).map_or("", |part| part.as_str());
    let mut micros = 0u32;
    for place in 0..6 {
        micros = micros * 10
            + fraction
                .as_bytes()
                .get(place)
                .map_or(0, |digit| u32::from(*digit - b'0'));
    }
    let date = day
        .and_hms_micro_opt(
            values[4].parse()?,
            values[5].parse()?,
            values[6].parse()?,
            micros,
        )
        .ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
    let mut stamp = date.format("%Y-%m-%d %H:%M:%S").to_string();
    if micros != 0 {
        stamp.push_str(&format!(".{micros:06}"));
    }
    if let Some(zone) = values.get(8) {
        if zone.as_str() == "Z" {
            stamp.push_str("+00:00");
        } else {
            let hour: u32 = values[10].parse()?;
            let minute: u32 = values.get(11).map_or(Ok(0), |part| part.as_str().parse())?;
            let offset = hour * 60 + minute;
            ensure!(offset < 24 * 60, "{INVALID}");
            let sign = if offset == 0 { "+" } else { &values[9] };
            stamp.push_str(&format!("{sign}{:02}:{:02}", offset / 60, offset % 60));
        }
    }
    Ok(Some(stamp))
}

struct Reader<'a> {
    parser: Parser<std::str::Chars<'a>>,
    nodes: usize,
    dates: HashSet<String>,
}

impl Reader<'_> {
    fn next(&mut self) -> Result<Event> {
        Ok(self
            .parser
            .next_token()
            .map_err(|_| anyhow::anyhow!("{INVALID}"))?
            .0)
    }

    fn is_end(&mut self, end: Event) -> Result<bool> {
        Ok(self
            .parser
            .peek()
            .map_err(|_| anyhow::anyhow!("{INVALID}"))?
            .0
            == end)
    }

    fn count_node(&mut self, pointer: &str, depth: usize) -> Result<()> {
        self.nodes += 1;
        ensure!(
            self.nodes <= MAX_UNITS && depth <= MAX_DEPTH && pointer.chars().count() <= 4096,
            "{STRUCTURE}"
        );
        Ok(())
    }

    fn value(&mut self, pointer: &str, depth: usize) -> Result<Value> {
        self.count_node(pointer, depth)?;
        match self.next()? {
            Event::Scalar(raw, style, 0, None) => {
                let (value, datetime) = scalar(raw, style)?;
                if datetime {
                    self.dates.insert(pointer.to_owned());
                }
                Ok(value)
            }
            Event::SequenceStart(0, None) => {
                ensure!(depth < MAX_DEPTH, "{STRUCTURE}");
                let mut values = Vec::new();
                while !self.is_end(Event::SequenceEnd)? {
                    let path = format!("{pointer}/{}", values.len());
                    values.push(self.value(&path, depth + 1)?);
                }
                self.next()?;
                Ok(Value::Array(values))
            }
            Event::MappingStart(0, None) => {
                ensure!(depth < MAX_DEPTH, "{STRUCTURE}");
                let mut values = Map::new();
                while !self.is_end(Event::MappingEnd)? {
                    self.count_node(pointer, depth + 1)?;
                    let Event::Scalar(raw, style, 0, None) = self.next()? else {
                        bail!("DOCUMENT_YAML：只支援不重複的字串 key；禁止 merge。");
                    };
                    let (key, datetime) = scalar(raw, style)?;
                    let Value::String(key) = key else {
                        bail!("{INVALID}");
                    };
                    ensure!(
                        !datetime && key != "<<" && !values.contains_key(&key),
                        "DOCUMENT_YAML：只支援不重複的字串 key；禁止 merge。"
                    );
                    let path = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                    values.insert(key, self.value(&path, depth + 1)?);
                }
                self.next()?;
                Ok(Value::Object(values))
            }
            Event::Scalar(..)
            | Event::Alias(..)
            | Event::MappingStart(..)
            | Event::SequenceStart(..) => bail!("{NODE_PROPERTIES}"),
            _ => bail!("{INVALID}"),
        }
    }
}

pub(super) fn parse(text: &str) -> Result<(Value, HashSet<String>)> {
    let mut reader = Reader {
        parser: Parser::new_from_str(text),
        nodes: 0,
        dates: HashSet::new(),
    };
    ensure!(reader.next()? == Event::StreamStart, "{INVALID}");
    match reader.next()? {
        Event::StreamEnd => return Ok((Value::Null, reader.dates)),
        Event::DocumentStart => {}
        _ => bail!("{INVALID}"),
    }
    let value = reader.value("", 0)?;
    ensure!(reader.next()? == Event::DocumentEnd, "{INVALID}");
    ensure!(
        reader.next()? == Event::StreamEnd,
        "DOCUMENT_YAML：一次只讀一份 YAML 文件。"
    );
    Ok((value, reader.dates))
}
