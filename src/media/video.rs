use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{INVALID, big, little, word};

const MAX_ELEMENTS: usize = 10_000;
const MAX_TRACKS: usize = 32;
const LIMIT: &str = "MEDIA_CONTAINER_LIMIT：影音容器項目、長度或深度超限。";

fn be16(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(data[at..at + 2].try_into().unwrap())
}

fn be64(data: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
}

fn fourcc(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii() {
                char::from(b)
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

fn require(data: &[u8], minimum: usize) -> Result<&[u8]> {
    ensure!(data.len() >= minimum, "{INVALID}");
    Ok(data)
}

type BoxList<'a> = Vec<(&'a [u8], &'a [u8])>;

pub(crate) fn boxes<'a>(mut data: &'a [u8], count: &mut usize) -> Result<BoxList<'a>> {
    let mut boxes = Vec::new();
    while !data.is_empty() {
        *count += 1;
        ensure!(*count <= MAX_ELEMENTS, "{LIMIT}");
        require(data, 8)?;
        let (length, header) = match big(data, 0) {
            0 => (data.len() as u64, 8),
            1 => (be64(require(data, 16)?, 8), 16),
            n => (u64::from(n), 8),
        };
        ensure!(
            (header as u64..=data.len() as u64).contains(&length),
            "{INVALID}"
        );
        let length = length as usize;
        boxes.push((&data[4..8], &data[header..length]));
        data = &data[length..];
    }
    Ok(boxes)
}

fn only_box<'a>(items: &BoxList<'a>, key: &[u8]) -> Result<&'a [u8]> {
    let mut found = items.iter().filter(|(name, _)| *name == key);
    let (_, data) = found.next().context(INVALID)?;
    ensure!(found.next().is_none(), "{INVALID}");
    Ok(data)
}

pub(super) fn mp4(data: &[u8]) -> Result<Value> {
    let mut count = 0;
    let top = boxes(data, &mut count)?;
    require(only_box(&top, b"ftyp")?, 8)?;
    let moov = boxes(only_box(&top, b"moov")?, &mut count)?;
    let mut tracks = Vec::new();
    let mut duration = 0_f64;
    for (_, trak) in moov.iter().filter(|(name, _)| *name == b"trak") {
        ensure!(tracks.len() < MAX_TRACKS, "{LIMIT}");
        let track = boxes(trak, &mut count)?;
        let mdia = boxes(only_box(&track, b"mdia")?, &mut count)?;
        let mdhd = require(only_box(&mdia, b"mdhd")?, 24)?;
        let (scale, ticks) = match mdhd[0] {
            0 => (big(mdhd, 12), u64::from(big(mdhd, 16))),
            1 => (big(require(mdhd, 36)?, 20), be64(mdhd, 24)),
            _ => bail!("{INVALID}"),
        };
        let seconds = (scale > 0).then(|| ticks as f64 / f64::from(scale));
        duration = duration.max(seconds.unwrap_or_default());
        let handler = &require(only_box(&mdia, b"hdlr")?, 12)?[8..12];
        let kind = match handler {
            b"vide" => "video",
            b"soun" => "audio",
            _ => "other",
        };
        let minf = boxes(only_box(&mdia, b"minf")?, &mut count)?;
        let stbl = boxes(only_box(&minf, b"stbl")?, &mut count)?;
        let descriptions = require(only_box(&stbl, b"stsd")?, 8)?;
        let entries = boxes(&descriptions[8..], &mut count)?;
        ensure!(
            (1..=MAX_TRACKS).contains(&entries.len())
                && entries.len() == big(descriptions, 4) as usize,
            "{INVALID}"
        );
        let codecs = entries
            .iter()
            .map(|(name, _)| fourcc(name))
            .collect::<Vec<_>>();
        let mut row = json!({"type":kind,"duration":seconds,"codecs":codecs});
        if handler == b"vide" {
            let sample = require(entries[0].1, 28)?;
            row["width"] = json!(be16(sample, 24));
            row["height"] = json!(be16(sample, 26));
        } else if handler == b"soun" {
            let sample = require(entries[0].1, 28)?;
            row["channels"] = json!(be16(sample, 16));
            row["sample_rate"] = json!(f64::from(big(sample, 24)) / 65536.);
        }
        if let Some((_, timing)) = stbl.iter().find(|(name, _)| *name == b"stts") {
            let records = big(require(timing, 8)?, 4) as usize;
            ensure!(
                records <= 100_000 && timing.len() == 8 + records * 8,
                "{LIMIT}"
            );
            if handler == b"vide" {
                let (mut frames, mut ticks) = (0_u64, 0_u128);
                for entry in timing[8..].chunks_exact(8) {
                    let amount = big(entry, 0);
                    frames += u64::from(amount);
                    ticks += u128::from(amount) * u128::from(big(entry, 4));
                }
                row["fps"] =
                    json!((ticks > 0).then(|| frames as f64 * f64::from(scale) / ticks as f64));
            }
        }
        tracks.push(row);
    }
    ensure!(!tracks.is_empty(), "{INVALID}");
    let mut tags = json!({});
    if let Some(track) = tracks.iter_mut().find(|track| track["type"] == "audio") {
        if let Ok((audio, metadata)) = super::audio::inspect_track(data, "MP4") {
            tags = metadata;
            for name in ["bitrate", "sample_rate", "channels"] {
                if !audio[name].is_null() {
                    track[name] = audio[name].clone();
                }
            }
        }
    }
    Ok(json!({
        "format":"MP4","tracks":tracks,"duration":duration,"tags":tags,
        "bitrate":(duration > 0.).then(|| data.len() as f64 * 8. / duration),
        "bitrate_kind":"whole-file average","fragmented":top.iter().any(|(name, _)| *name == b"moof")
    }))
}

fn avi_chunks(
    mut data: &[u8],
    count: &mut usize,
    depth: usize,
    visit: &mut impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    ensure!(depth <= 8, "{LIMIT}");
    while !data.is_empty() {
        *count += 1;
        ensure!(*count <= MAX_ELEMENTS, "{LIMIT}");
        let length = little(require(data, 8)?, 4) as usize;
        let next = 8 + length + (length % 2);
        require(data, next)?;
        let name = &data[..4];
        let payload = &data[8..8 + length];
        if name == b"LIST" && &require(payload, 4)?[..4] != b"movi" {
            avi_chunks(&payload[4..], count, depth + 1, visit)?;
        } else {
            visit(name, payload)?;
        }
        data = &data[next..];
    }
    Ok(())
}

pub(super) fn avi(data: &[u8]) -> Result<Value> {
    ensure!(
        require(data, 12)?.len() == little(data, 4) as usize + 8,
        "{INVALID}"
    );
    let mut header = None;
    let mut tracks = Vec::<Value>::new();
    avi_chunks(&data[12..], &mut 0, 0, &mut |kind, body| {
        match kind {
            b"avih" => {
                require(body, 56)?;
                ensure!(header.is_none(), "{INVALID}");
                header = Some((
                    little(body, 0),
                    little(body, 4),
                    little(body, 16),
                    little(body, 32),
                    little(body, 36),
                ));
            }
            b"strh" => {
                require(body, 56)?;
                ensure!(tracks.len() < MAX_TRACKS, "{LIMIT}");
                let scale = f64::from(little(body, 20));
                let rate = f64::from(little(body, 24));
                let length = f64::from(little(body, 32));
                let kind = match &body[..4] {
                    b"vids" => "video",
                    b"auds" => "audio",
                    _ => "other",
                };
                tracks.push(json!({
                    "type":kind,"codec":fourcc(&body[4..8]),"duration":(rate > 0.).then(|| length * scale / rate),
                    "fps":(scale > 0. && kind == "video").then(|| rate / scale)
                }));
            }
            b"strf" => {
                if let Some(track) = tracks.last_mut().filter(|track| track["type"] == "audio") {
                    require(body, 16)?;
                    track["codec"] = json!(format!("WAVE:{}", word(body, 0)));
                    track["channels"] = json!(word(body, 2));
                    track["sample_rate"] = json!(little(body, 4));
                    track["bitrate"] = json!(u64::from(little(body, 8)) * 8);
                    track["bits_per_sample"] = json!(word(body, 14));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    let (frame_time, max_bytes, frames, width, height) = header.context(INVALID)?;
    for track in tracks.iter_mut().filter(|track| track["type"] == "video") {
        track["width"] = json!(width);
        track["height"] = json!(height);
    }
    Ok(json!({
        "format":"AVI","tracks":tracks,"duration":f64::from(frames) * f64::from(frame_time) / 1_000_000.,
        "width":width,"height":height,"fps":(frame_time > 0).then(|| 1_000_000. / f64::from(frame_time)),
        "tags":{},"bitrate":u64::from(max_bytes) * 8,"bitrate_kind":"declared maximum"
    }))
}

const EBML: u32 = 0x1a45dfa3;
const SEGMENT: u32 = 0x18538067;
const INFO: u32 = 0x1549a966;
const TRACKS: u32 = 0x1654ae6b;
const TRACK_ENTRY: u32 = 0xae;
const EBML_INVALID: &str = "MEDIA_EBML：EBML 元素截斷、重複或無效。";
type Fields<'a> = BTreeMap<u32, &'a [u8]>;

fn uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |acc, &byte| (acc << 8) | u64::from(byte))
}

fn vint(bytes: &[u8], id: bool) -> Result<(u64, usize, bool)> {
    let first = *bytes.first().context(EBML_INVALID)?;
    let width = first.leading_zeros() as usize + 1;
    ensure!(width <= if id { 4 } else { 8 }, "{EBML_INVALID}");
    let raw = uint(bytes.get(..width).context(EBML_INVALID)?);
    let mask = (1_u64 << (7 * width)) - 1;
    Ok(if id {
        (raw, width, false)
    } else {
        (raw & mask, width, raw & mask == mask)
    })
}

fn elements<'a>(mut data: &'a [u8], count: &mut usize, top: bool) -> Result<Vec<(u32, &'a [u8])>> {
    let mut values = Vec::new();
    while !data.is_empty() {
        *count += 1;
        ensure!(*count <= MAX_ELEMENTS, "{LIMIT}");
        let (id, tag_width, _) = vint(data, true)?;
        let (length, size_width, unknown) = vint(&data[tag_width..], false)?;
        let id = id as u32;
        data = &data[tag_width + size_width..];
        let length = if unknown {
            ensure!(top && id == SEGMENT, "{EBML_INVALID}");
            data.len()
        } else {
            ensure!(length <= data.len() as u64, "{EBML_INVALID}");
            length as usize
        };
        values.push((id, &data[..length]));
        data = &data[length..];
    }
    Ok(values)
}

fn fields<'a>(entries: impl IntoIterator<Item = (u32, &'a [u8])>) -> Result<Fields<'a>> {
    let mut fields = Fields::new();
    for (id, data) in entries {
        if !matches!(id, 0xec | 0xbf) {
            ensure!(fields.insert(id, data).is_none(), "{EBML_INVALID}");
        }
    }
    Ok(fields)
}

fn value<'a>(fields: &Fields<'a>, id: u32, max: usize) -> Result<Option<&'a [u8]>> {
    let bytes = fields.get(&id).copied();
    ensure!(
        bytes.is_none_or(|bytes| bytes.len() <= max),
        "{EBML_INVALID}"
    );
    Ok(bytes)
}

fn ebml_uint(fields: &Fields<'_>, id: u32) -> Result<Option<u64>> {
    Ok(value(fields, id, 8)?.map(uint))
}

fn ebml_float(fields: &Fields<'_>, id: u32) -> Result<Option<f64>> {
    let Some(bytes) = value(fields, id, 8)? else {
        return Ok(None);
    };
    let value = match bytes.len() {
        4 => f64::from(f32::from_bits(big(bytes, 0))),
        8 => f64::from_bits(be64(bytes, 0)),
        _ => bail!("{EBML_INVALID}"),
    };
    ensure!(value.is_finite() && value >= 0., "{EBML_INVALID}");
    Ok(Some(value))
}

fn ebml_ascii(bytes: &[u8]) -> Result<&str> {
    ensure!(bytes.is_ascii(), "{EBML_INVALID}");
    Ok(std::str::from_utf8(bytes)?)
}

pub(super) fn matroska(data: &[u8]) -> Result<Value> {
    let mut count = 0;
    let top = fields(elements(data, &mut count, true)?)?;
    let header = fields(elements(
        top.get(&EBML).context(EBML_INVALID)?,
        &mut count,
        false,
    )?)?;
    let format = match value(&header, 0x4282, 256)?.context(EBML_INVALID)? {
        b"matroska" => "MATROSKA",
        b"webm" => "WEBM",
        _ => bail!("{EBML_INVALID}"),
    };
    let segment = elements(top.get(&SEGMENT).context(EBML_INVALID)?, &mut count, false)?;
    let selected = fields(
        segment
            .into_iter()
            .filter(|(tag, _)| matches!(*tag, INFO | TRACKS)),
    )?;
    let info = fields(elements(
        selected.get(&INFO).context(EBML_INVALID)?,
        &mut count,
        false,
    )?)?;
    let scale = ebml_uint(&info, 0x2ad7b1)?.unwrap_or(1_000_000);
    ensure!(scale > 0, "{EBML_INVALID}");
    let duration = ebml_float(&info, 0x4489)?.map(|units| units * (scale as f64 / 1_000_000_000.));
    ensure!(duration.is_none_or(f64::is_finite), "{EBML_INVALID}");
    let mut tracks = Vec::new();
    for (_, bytes) in elements(
        selected.get(&TRACKS).context(EBML_INVALID)?,
        &mut count,
        false,
    )?
    .into_iter()
    .filter(|(tag, _)| *tag == TRACK_ENTRY)
    {
        ensure!(tracks.len() < MAX_TRACKS, "{LIMIT}");
        let track = fields(elements(bytes, &mut count, false)?)?;
        let codec = ebml_ascii(value(&track, 0x86, 256)?.context(EBML_INVALID)?)?;
        let language = ebml_ascii(
            value(&track, 0x22b59c, 256)?
                .filter(|bytes| !bytes.is_empty())
                .unwrap_or(b"eng"),
        )?;
        let kind = match ebml_uint(&track, 0x83)? {
            Some(1) => "video",
            Some(2) => "audio",
            Some(17) => "subtitle",
            _ => "other",
        };
        let mut row = json!({"number":ebml_uint(&track, 0xd7)?,"type":kind,"codec":codec,"language":language});
        if let Some(video) = track.get(&0xe0) {
            let video = fields(elements(video, &mut count, false)?)?;
            row["width"] = json!(ebml_uint(&video, 0xb0)?);
            row["height"] = json!(ebml_uint(&video, 0xba)?);
        }
        if let Some(audio) = track.get(&0xe1) {
            let audio = fields(elements(audio, &mut count, false)?)?;
            row["sample_rate"] = json!(ebml_float(&audio, 0xb5)?.unwrap_or(8000.));
            row["channels"] = json!(ebml_uint(&audio, 0x9f)?.unwrap_or(1));
            row["bits_per_sample"] = json!(ebml_uint(&audio, 0x6264)?);
        }
        tracks.push(row);
    }
    ensure!(!tracks.is_empty(), "{EBML_INVALID}");
    Ok(json!({
        "format":format,"tracks":tracks,"duration":duration,"tags":{},
        "validation":"bounded metadata only; frames and EBML CRC not validated"
    }))
}
