use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use crate::{formats::sniff, images};

mod audio;
mod video;

pub(crate) use video::boxes as isobmff_boxes;

const INVALID: &str = "MEDIA_CONTAINER：容器截斷、結構無效或編碼不支援。";
pub const FORMATS: &[&str] = &[
    "MP3",
    "FLAC",
    "OGG",
    "WAV",
    "MP4",
    "AVI",
    "AIFF",
    "AU",
    "MATROSKA",
    "WEBM",
    "static image metadata (see image_limits)",
];

fn big(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn little(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn word(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

pub fn supports(format: &str) -> bool {
    matches!(
        format,
        "MPEG_AUDIO" | "PNG" | "JPEG" | "WEBP" | "GIF" | "BMP" | "TIFF" | "ICO" | "PPM" | "AVIF"
    ) || FORMATS.contains(&format)
}

pub fn inspect(data: &[u8]) -> Result<Value> {
    let format = sniff(data).0;
    match format {
        "PNG" | "JPEG" | "WEBP" | "GIF" | "BMP" | "TIFF" | "ICO" | "PPM" | "AVIF" => {
            let mut result = images::decode(data, format)?["metadata"].clone();
            result["format"] = json!(format);
            result["kind"] = json!("static image");
            Ok(result)
        }
        "AU" => au(data),
        "WAV" => wav(data),
        "FLAC" => flac(data),
        "AIFF" | "MP3" | "MPEG_AUDIO" | "OGG" => audio::inspect(data, format),
        "MP4" => video::mp4(data),
        "AVI" => video::avi(data),
        "MATROSKA" => video::matroska(data),
        _ => bail!("MEDIA_FORMAT：不支援此影音格式。"),
    }
}

fn au(data: &[u8]) -> Result<Value> {
    ensure!(data.len() >= 24, "{INVALID}");
    let offset = big(data, 4) as usize;
    let size = big(data, 8);
    let encoding = big(data, 12);
    let rate = big(data, 16);
    let channels = big(data, 20);
    let bits = match encoding {
        1 | 2 | 27 => 8,
        3 => 16,
        4 => 24,
        5 | 6 => 32,
        7 => 64,
        _ => bail!("{INVALID}"),
    };
    ensure!(
        (24..=data.len()).contains(&offset) && rate > 0 && (1..=32).contains(&channels),
        "{INVALID}"
    );
    let bytes = data.len() - offset;
    let frame = (channels * (bits / 8)) as usize;
    ensure!(
        (size == u32::MAX || size as usize == bytes) && bytes % frame == 0,
        "{INVALID}"
    );
    Ok(json!({
        "format": "AU", "tags": {}, "tracks": [{
            "type": "audio", "codec": format!("AU:{encoding}"), "bits_per_sample": bits,
            "sample_rate": rate, "channels": channels, "duration": (bytes / frame) as f64 / f64::from(rate)
        }]
    }))
}

fn wav(data: &[u8]) -> Result<Value> {
    ensure!(
        data.len() >= 12 && little(data, 4) as usize + 8 == data.len(),
        "{INVALID}"
    );
    let mut offset = 12;
    let mut format = None;
    let mut payload = None;
    let mut has_id3 = false;
    for _ in 0..10_000 {
        if offset == data.len() {
            break;
        }
        let header = data
            .get(offset..offset + 8)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let len = little(header, 4) as usize;
        let body = data
            .get(offset + 8..offset + 8 + len)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        match &header[..4] {
            b"fmt " => {
                ensure!(format.is_none() && body.len() >= 16, "{INVALID}");
                match word(body, 0) {
                    1 | 3 => {}
                    0xfffe => validate_wave_extension(body)?,
                    _ => bail!("{INVALID}"),
                }
                format = Some((
                    word(body, 2),
                    little(body, 4),
                    little(body, 8),
                    word(body, 12),
                    word(body, 14),
                ));
            }
            b"data" => {
                ensure!(payload.is_none(), "{INVALID}");
                payload = Some(len);
            }
            b"ID3 " | b"id3 " => {
                ensure!(!has_id3, "{INVALID}");
                validate_wave_id3_chunk(body)?;
                has_id3 = true;
            }
            _ => {}
        }
        offset += 8 + len + (len % 2);
    }
    let (channels, rate, byte_rate, alignment, bits) =
        format.ok_or_else(|| anyhow::anyhow!(INVALID))?;
    let payload = payload.ok_or_else(|| anyhow::anyhow!(INVALID))?;
    ensure!(
        offset == data.len()
            && (1..=32).contains(&channels)
            && rate > 0
            && alignment > 0
            && bits > 0
            && payload % usize::from(alignment) == 0
            && u64::from(rate) * u64::from(alignment) == u64::from(byte_rate),
        "{INVALID}"
    );
    let tags = if has_id3 {
        audio::wave_id3(data)?
    } else {
        Default::default()
    };
    Ok(json!({"format": "WAV", "tags": tags, "tracks": [{
        "type": "audio", "codec": "WAVE", "sample_rate": rate, "channels": channels, "bits_per_sample": bits,
        "bitrate": u64::from(rate) * u64::from(channels) * u64::from(bits),
        "duration": payload as f64 / f64::from(byte_rate)
    }]}))
}

fn validate_wave_extension(body: &[u8]) -> Result<()> {
    // KSDATAFORMAT_SUBTYPE_PCM and KSDATAFORMAT_SUBTYPE_IEEE_FLOAT share
    // this GUID suffix and use their WAVE format tag as the first DWORD.
    const GUID_SUFFIX: &[u8; 12] = b"\0\0\x10\0\x80\0\0\xaa\0\x38\x9b\x71";
    ensure!(body.len() >= 40, "{INVALID}");
    let extension_size = usize::from(word(body, 16));
    let channels = word(body, 2);
    let bits = word(body, 14);
    let valid_bits = word(body, 18);
    let channel_mask = little(body, 20);
    let subtype = little(body, 24);
    ensure!(
        (22..=body.len() - 18).contains(&extension_size)
            && &body[28..40] == GUID_SUFFIX
            && matches!(subtype, 1 | 3)
            && (1..=bits).contains(&valid_bits)
            && bits % 8 == 0
            && u32::from(word(body, 12)) == u32::from(channels) * u32::from(bits / 8)
            && (channel_mask == 0 || channel_mask.count_ones() == u32::from(channels))
            && (subtype != 3 || matches!(bits, 32 | 64) && valid_bits == bits),
        "{INVALID}"
    );
    Ok(())
}

fn validate_wave_id3_chunk(body: &[u8]) -> Result<()> {
    const HEADER: usize = 10;
    let header = body.get(..HEADER).ok_or_else(|| anyhow::anyhow!(INVALID))?;
    let length = &header[6..10];
    ensure!(
        header.starts_with(b"ID3") && length.iter().all(|byte| *byte < 128),
        "{INVALID}"
    );
    let size = length
        .iter()
        .fold(0_usize, |size, byte| (size << 7) | usize::from(*byte));
    let footer_size = if header[3] == 4 && header[5] & 0x10 != 0 {
        HEADER
    } else {
        0
    };
    ensure!(HEADER + size + footer_size <= body.len(), "{INVALID}");
    Ok(())
}

fn flac(data: &[u8]) -> Result<Value> {
    ensure!(
        data.len() >= 42 && data[4] & 0x7f == 0 && big(data, 4) & 0xff_ffff == 34,
        "{INVALID}"
    );
    let info = u64::from_be_bytes(data[18..26].try_into().unwrap());
    let rate = info >> 44;
    let channels = (info >> 41) & 7;
    let bits = (info >> 36) & 31;
    let samples = info & ((1 << 36) - 1);
    ensure!(rate > 0 && bits >= 3, "{INVALID}");
    let mut offset = 4;
    let mut last = false;
    let mut tags = serde_json::Map::new();
    for _ in 0..10_000 {
        let header = data
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let length = (big(header, 0) & 0xff_ffff) as usize;
        let block = data
            .get(offset + 4..offset + 4 + length)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        if header[0] & 0x7f == 4 {
            vorbis(block, &mut tags)?;
        }
        offset += 4 + length;
        if header[0] & 0x80 != 0 {
            last = true;
            break;
        }
    }
    ensure!(last, "{INVALID}");
    let duration = samples as f64 / rate as f64;
    Ok(json!({"format": "FLAC", "tags": tags, "tracks": [{
        "type": "audio", "codec": "FLAC", "sample_rate": rate, "channels": channels + 1, "bits_per_sample": bits + 1,
        "bitrate": if duration == 0. { 0 } else { ((data.len() - offset) as f64 * 8. / duration) as u64 },
        "duration": duration
    }]}))
}

fn vorbis(data: &[u8], tags: &mut serde_json::Map<String, Value>) -> Result<()> {
    ensure!(data.len() >= 8, "{INVALID}");
    let mut offset = 4 + little(data, 0) as usize;
    let header = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow::anyhow!(INVALID))?;
    let entries = little(header, 0) as usize;
    ensure!(entries <= data.len() / 4, "{INVALID}");
    offset += 4;
    for _ in 0..entries {
        let header = data
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        let length = little(header, 0) as usize;
        let entry = data
            .get(offset + 4..offset + 4 + length)
            .ok_or_else(|| anyhow::anyhow!(INVALID))?;
        offset += 4 + length;
        if let Some((key, value)) = std::str::from_utf8(entry)?.split_once('=') {
            let key: String = key.chars().take(128).flat_map(char::to_lowercase).collect();
            if tags.len() < 32 || tags.contains_key(&key) {
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
    }
    Ok(())
}
