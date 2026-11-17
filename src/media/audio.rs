use std::io::Cursor;

use anyhow::{Result, bail, ensure};
use lofty::{
    config::{GlobalOptions, ParseOptions, ParsingMode, apply_global_options},
    file::{AudioFile, FileType, TaggedFileExt},
    id3::v2::Frame,
    iff::wav::WavFile,
    probe::Probe,
    tag::TagType,
};
use serde_json::{Map, Value, json};

use super::{INVALID, big};

fn metadata_options() -> ParseOptions {
    apply_global_options(GlobalOptions::new().allocation_limit(8 * 1024 * 1024));
    ParseOptions::new()
        .parsing_mode(ParsingMode::Strict)
        .read_cover_art(false)
}

pub(super) fn wave_id3(data: &[u8]) -> Result<Map<String, Value>> {
    let parsed = WavFile::read_from(
        &mut Cursor::new(data),
        metadata_options().read_properties(false),
    )
    .map_err(|_| anyhow::anyhow!(INVALID))?;
    let mut tags = Map::new();
    if let Some(tag) = parsed.id3v2() {
        for frame in tag.iter() {
            let id = frame.id_str();
            let (key, text, multiple) = match frame {
                Frame::Text(frame) => (id.to_owned(), frame.value.as_ref(), true),
                Frame::UserText(frame) => (
                    format!("{id}:{}", frame.description),
                    frame.content.as_ref(),
                    true,
                ),
                Frame::Comment(frame) => (
                    format!(
                        "{id}:{}:{}",
                        frame.description,
                        String::from_utf8_lossy(&frame.language)
                    ),
                    frame.content.as_ref(),
                    true,
                ),
                Frame::UnsynchronizedText(frame) => (
                    format!(
                        "{id}:{}:{}",
                        frame.description,
                        String::from_utf8_lossy(&frame.language)
                    ),
                    frame.content.as_ref(),
                    false,
                ),
                _ => continue,
            };
            let values: Vec<String> = if multiple {
                text.trim_end_matches('\0')
                    .split('\0')
                    .take(4)
                    .map(|value| value.chars().take(1024).collect())
                    .collect()
            } else {
                vec![text.chars().take(1024).collect()]
            };
            tags.insert(key.chars().take(128).collect::<String>(), json!(values));
            if tags.len() == 32 {
                break;
            }
        }
    }
    Ok(tags)
}

pub(super) fn inspect_track(data: &[u8], format: &str) -> Result<(Value, Value)> {
    let options = metadata_options();
    let file_type = match format {
        "AIFF" => {
            ensure!(
                data.len() >= 12 && big(data, 4) as usize + 8 == data.len(),
                "{INVALID}"
            );
            FileType::Aiff
        }
        "MP4" => FileType::Mp4,
        "MP3" | "MPEG_AUDIO" => FileType::Mpeg,
        "OGG" => match Probe::new(Cursor::new(data)).guess_file_type()?.file_type() {
            Some(kind @ (FileType::Vorbis | FileType::Opus)) => kind,
            _ => bail!("{INVALID}"),
        },
        _ => bail!("{INVALID}"),
    };
    let parsed = Probe::with_file_type(Cursor::new(data), file_type)
        .options(options)
        .read()?;
    let properties = parsed.properties();
    let channels = properties.channels();
    let rate = properties.sample_rate();
    ensure!(
        channels.is_some_and(|count| (1..=32).contains(&count))
            && rate.is_some_and(|rate| rate > 0),
        "{INVALID}"
    );
    let (codec, rate) = match file_type {
        FileType::Aiff => ("AIFF", rate),
        FileType::Mpeg => ("MP3", rate),
        FileType::Mp4 => ("MP4", rate),
        FileType::Opus => ("Opus", Some(48_000)),
        FileType::Vorbis => ("OggVorbis", rate),
        _ => unreachable!(),
    };
    let bitrate = match file_type {
        FileType::Opus => None,
        FileType::Aiff => {
            rate.zip(channels)
                .zip(properties.bit_depth())
                .map(|((rate, channels), bits)| {
                    u64::from(rate) * u64::from(channels) * u64::from(bits)
                })
        }
        _ => properties
            .audio_bitrate()
            .map(|kbps| u64::from(kbps) * 1_000),
    };
    let mut tags = Map::new();
    if let Some(tag) = parsed.primary_tag().or_else(|| parsed.first_tag()) {
        let kind = tag.tag_type();
        for item in tag.items() {
            let (Some(key), Some(value)) = (item.key().map_key(kind), item.value().text()) else {
                continue;
            };
            let key: String = if kind == TagType::VorbisComments {
                key.chars().take(128).flat_map(char::to_lowercase).collect()
            } else {
                key.chars().take(128).collect()
            };
            if tags.len() >= 32 && !tags.contains_key(&key) {
                continue;
            }
            let values = tags
                .entry(key)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap();
            if values.len() < 4 {
                values.push(json!(value.chars().take(1024).collect::<String>()));
            }
        }
    }
    Ok((
        json!({
            "type":"audio","codec":codec,"duration":properties.duration().as_secs_f64(),
            "sample_rate":rate,"channels":channels,"bitrate":bitrate,"bits_per_sample":properties.bit_depth()
        }),
        Value::Object(tags),
    ))
}

pub(super) fn inspect(data: &[u8], format: &str) -> Result<Value> {
    let (track, tags) = inspect_track(data, format)?;
    Ok(json!({"format":format,"tracks":[track],"tags":tags}))
}
