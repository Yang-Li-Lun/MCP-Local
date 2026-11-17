use std::io::{self, Cursor, Write};

use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
    codecs::{
        gif::GifDecoder,
        png::{PngDecoder, PngEncoder},
        webp::WebPDecoder,
    },
    imageops::FilterType,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

use crate::{
    budget::Budget,
    filesystem::Root,
    formats::{extension, sniff},
    parser::{self, Operation},
};

mod avif_grid;

const MAX_SOURCE: usize = 16 * 1024 * 1024;
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 25_000_000;
const MAX_OUTPUT_SIDE: u32 = 2048;
const MAX_OUTPUT: usize = 3 * 1024 * 1024;

pub fn limits() -> Value {
    let base64_limit = 4 * MAX_OUTPUT.div_ceil(3);
    json!({
        "formats": ["PNG", "JPEG", "WebP", "GIF", "BMP", "TIFF", "ICO", "PNM (P1-P6)", "AVIF"],
        "output_mime_type": "image/png", "avif_available": true,
        "max_source_bytes": MAX_SOURCE, "source_bytes_policy": "min(max_source_bytes, root.max_file_bytes, root.max_scan_bytes)",
        "max_source_side": MAX_SIDE, "max_source_pixels": MAX_PIXELS, "max_output_side": MAX_OUTPUT_SIDE,
        "max_image_bytes": MAX_OUTPUT, "max_base64_bytes": base64_limit,
        "max_response_bytes": base64_limit + 64 * 1024, "animation": "rejected", "metadata": "stripped",
        "resize": "aspect ratio preserved; never upscale", "parser_timeout_seconds": 10,
        "parser_memory_bytes": parser::MAX_MEMORY
    })
}

pub fn expected(path: &str) -> Option<&'static str> {
    match extension(path).as_str() {
        ".png" => Some("PNG"),
        ".jpg" | ".jpeg" => Some("JPEG"),
        ".webp" => Some("WEBP"),
        ".gif" => Some("GIF"),
        ".bmp" => Some("BMP"),
        ".tif" | ".tiff" => Some("TIFF"),
        ".ico" => Some("ICO"),
        ".avif" => Some("AVIF"),
        ".pbm" | ".pgm" | ".ppm" | ".pnm" => Some("PPM"),
        _ => None,
    }
}

pub fn read(root: &Root, path: &str, budget: &mut Budget) -> Result<CallToolResult> {
    let file = root.open(path, true)?;
    let format = expected(&file.relative)
        .ok_or_else(|| anyhow::anyhow!("圖片僅支援 image_limits 公開且符合實際內容的副檔名。"))?;
    let maximum = MAX_SOURCE
        .min(root.settings.max_file_bytes)
        .min(root.settings.max_scan_bytes);
    ensure!(
        file.metadata().size <= maximum as u64,
        "圖片超過來源容量上限。"
    );
    let bytes = file.read(0, maximum, budget)?;
    let output = parser::parse(
        Operation::Image {
            format: format.into(),
        },
        bytes,
        budget,
    )?;
    let data = output["png"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("圖片解析結果無效。"))?;
    ensure!(
        data.len() <= 4 * MAX_OUTPUT.div_ceil(3),
        "IMAGE_OUTPUT_LIMIT：圖片回傳超過傳輸預算。"
    );
    Ok(CallToolResult::success(vec![
        ContentBlock::text(serde_json::to_string(&output["metadata"])?),
        ContentBlock::image(data, "image/png"),
    ]))
}

fn supported_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0
            && height > 0
            && width.max(height) <= MAX_SIDE
            && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "圖片解析度或像素數超出限制。"
    );
    Ok(())
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits
}

fn decode_pixels(source: &[u8], format: ImageFormat) -> Result<(DynamicImage, (u32, u32))> {
    let input = Cursor::new(source);
    match format {
        ImageFormat::Png => ensure!(
            !PngDecoder::with_limits(input, decoder_limits())?.is_apng()?,
            "不支援動畫或多幀圖片。"
        ),
        ImageFormat::WebP => ensure!(
            !WebPDecoder::new(input)?.has_animation(),
            "不支援動畫或多幀圖片。"
        ),
        ImageFormat::Tiff => ensure!(
            !tiff::decoder::Decoder::new(input)?.more_images(),
            "不支援動畫或多幀圖片。"
        ),
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(input)?;
            let (width, height) = decoder.dimensions();
            supported_dimensions(width, height)?;
            decoder.set_limits(decoder_limits())?;
            let mut frames = decoder.into_frames();
            let first = frames
                .next()
                .ok_or_else(|| anyhow::anyhow!("缺少 GIF 影格。"))??;
            ensure!(frames.next().is_none(), "不支援動畫或多幀圖片。");
            return Ok((
                DynamicImage::ImageRgba8(first.into_buffer()),
                (width, height),
            ));
        }
        _ => {}
    }
    let mut reader = ImageReader::with_format(Cursor::new(source), format);
    reader.limits(decoder_limits());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    supported_dimensions(width, height)?;
    let orientation = decoder.orientation()?;
    let mut decoded = DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    Ok((decoded, (width, height)))
}

const AVIF_DECODE_MEMORY: usize = 256 * 1024 * 1024;

fn native_avif_limits(memory: usize) -> Result<avif_rust::NativeDecodeLimits> {
    Ok(avif_rust::NativeDecodeLimits::new(
        MAX_SOURCE,
        MAX_SIDE as usize,
        MAX_SIDE as usize,
        MAX_PIXELS as usize,
        8 * MAX_PIXELS as usize,
        4 * 1024 * 1024,
        4 * 1024 * 1024,
        4096,
        4096,
        4096,
        64,
        1,
    )
    .with_max_live_allocation_bytes(memory)?)
}

fn decode_avif(source: &[u8]) -> Result<(DynamicImage, (u32, u32))> {
    if let Some(image) = avif_grid::decode(source)? {
        return Ok(image);
    }
    let limits = native_avif_limits(AVIF_DECODE_MEMORY)?;
    let decoded = avif_rust::decode_frame_bytes_strict_with_limits(source, &limits)?;
    let info = decoded.information().info();
    let orientation = avif_orientation(
        info.rotation.map_or(0, |rotation| rotation.angle),
        info.mirror.map(|mirror| mirror.axis),
    )?;
    let mut premultiplied = info.alpha_premultiplied;
    // The decoder projects the legacy property but omits the standard item reference.
    if let (Some(primary), [alpha]) = (info.primary_item_id, info.alpha_auxiliary_items.as_slice())
    {
        premultiplied |= avif_premultiplied(source, primary, alpha.item_id)?;
    }
    let mut frame = decoded.into_frame();
    frame.alpha_premultiplied = premultiplied;
    let (mut pixels, dimensions) = avif_frame_to_image(frame)?;
    pixels.apply_orientation(orientation);
    Ok((pixels, dimensions))
}

fn avif_orientation(angle: u8, mirror: Option<u8>) -> Result<image::metadata::Orientation> {
    // HEIF rotates counterclockwise before mirroring, the same order Pillow maps to EXIF.
    const EXIF: [[u8; 3]; 4] = [[1, 4, 2], [8, 5, 7], [3, 2, 4], [6, 7, 5]];
    ensure!(
        angle <= 3 && mirror.is_none_or(|axis| axis <= 1),
        "AVIF 方向資料無效。"
    );
    let exif = EXIF[usize::from(angle)][mirror.map_or(0, |axis| usize::from(axis) + 1)];
    Ok(image::metadata::Orientation::from_exif(exif).expect("fixed HEIF-to-EXIF orientation"))
}

fn avif_frame_to_image(mut frame: avif_rust::DecodedFrame) -> Result<(DynamicImage, (u32, u32))> {
    let dimensions = (u32::try_from(frame.width)?, u32::try_from(frame.height)?);
    supported_dimensions(dimensions.0, dimensions.1)?;
    // Pillow's AVIF decoder converts YUV to encoded RGB. The source reader then
    // strips ICC metadata without applying a display transfer or profile.
    let color =
        frame
            .color_config
            .color_description
            .get_or_insert(avif_rust::av1::ColorDescription {
                color_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 6,
            });
    color.transfer_characteristics = 2; // unspecified CICP transfer
    if color.matrix_coefficients == 2 {
        color.matrix_coefficients = 6; // libavif's BT.601 fallback
    }
    frame.color_information = None;
    // avif-rust 0.1.2 indexes alpha as the fourth plane during conversion.
    // A monochrome master stores it after the luma plane, with no chroma planes.
    let restore_alpha = frame.color_config.monochrome;
    let premultiplied = frame.alpha_premultiplied;
    frame.alpha_premultiplied = premultiplied && !restore_alpha;
    let mut rgba = frame.to_rgba8()?;
    let output = (u32::try_from(rgba.width)?, u32::try_from(rgba.height)?);
    supported_dimensions(output.0, output.1)?;
    let alpha = frame
        .buffers
        .planes
        .iter()
        .find(|plane| plane.layout.plane == 3);
    if let Some(alpha) = alpha.filter(|_| restore_alpha) {
        ensure!(
            alpha.layout.width == rgba.width
                && alpha.layout.height == rgba.height
                && alpha.samples.len() == rgba.rgba.len() / 4
                && matches!(frame.bit_depth, 8 | 10 | 12 | 16),
            "AVIF 透明度資料不完整。"
        );
        let max_sample = (1_u32 << frame.bit_depth) - 1;
        for (pixel, &sample) in rgba
            .rgba
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&alpha.samples)
        {
            let sample = u32::from(sample).min(max_sample);
            pixel[3] = ((sample * 255 + max_sample / 2) / max_sample) as u8;
            if premultiplied {
                for channel in &mut pixel[..3] {
                    *channel = (u32::from(*channel) * max_sample + sample / 2)
                        .checked_div(sample)
                        .unwrap_or(0)
                        .min(255) as u8;
                }
            }
        }
    }
    let has_alpha = alpha.is_some();
    drop(frame);
    let image = image::RgbaImage::from_raw(output.0, output.1, rgba.rgba)
        .ok_or_else(|| anyhow::anyhow!("AVIF 像素資料不完整。"))?;
    let image = DynamicImage::ImageRgba8(image);
    Ok((
        if has_alpha {
            image
        } else {
            DynamicImage::ImageRgb8(image.into_rgb8())
        },
        dimensions,
    ))
}

fn avif_premultiplied(source: &[u8], primary: u32, alpha: u32) -> Result<bool> {
    use crate::media::isobmff_boxes as boxes;
    const INVALID_REFERENCE: &str = "AVIF 透明度參照無效。";

    let mut count = 0;
    let top = boxes(source, &mut count)?;
    let metadata = top
        .iter()
        .find_map(|(kind, body)| (*kind == b"meta").then_some(*body))
        .and_then(|body| body.get(4..))
        .ok_or_else(|| anyhow::anyhow!(INVALID_REFERENCE))?;
    let mut premultiplied = false;
    for (_, references) in boxes(metadata, &mut count)?
        .into_iter()
        .filter(|(kind, _)| *kind == b"iref")
    {
        let (header, body) = references
            .split_at_checked(4)
            .ok_or_else(|| anyhow::anyhow!(INVALID_REFERENCE))?;
        let id_width = match header[0] {
            0 => 2,
            1 => 4,
            _ => bail!(INVALID_REFERENCE),
        };
        let read_id = |bytes: &[u8]| -> u32 {
            match bytes {
                [a, b] => u32::from(u16::from_be_bytes([*a, *b])),
                [a, b, c, d] => u32::from_be_bytes([*a, *b, *c, *d]),
                _ => unreachable!("item ID width is fixed by iref version"),
            }
        };
        for (_, reference) in boxes(body, &mut count)?
            .into_iter()
            .filter(|(kind, _)| *kind == b"prem")
        {
            let (from, targets) = reference
                .split_at_checked(id_width)
                .ok_or_else(|| anyhow::anyhow!(INVALID_REFERENCE))?;
            if read_id(from) != primary {
                continue;
            }
            ensure!(
                !premultiplied
                    && targets.len() == 2 + id_width
                    && targets[..2] == [0, 1]
                    && read_id(&targets[2..]) == alpha,
                "{INVALID_REFERENCE}"
            );
            premultiplied = true;
        }
    }
    Ok(premultiplied)
}

#[derive(Default)]
struct PngOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for PngOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + buffer.len() > MAX_OUTPUT {
            self.exceeded = true;
            return Err(io::Error::other("PNG output limit"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn decode(source: &[u8], expected: &str) -> Result<Value> {
    ensure!(
        source.len() <= MAX_SOURCE && sniff(source).0 == expected,
        "圖片格式不符。"
    );
    let format = match expected {
        "PNG" => ImageFormat::Png,
        "JPEG" => ImageFormat::Jpeg,
        "WEBP" => ImageFormat::WebP,
        "GIF" => ImageFormat::Gif,
        "BMP" => ImageFormat::Bmp,
        "TIFF" => ImageFormat::Tiff,
        "ICO" => ImageFormat::Ico,
        "PPM" => ImageFormat::Pnm,
        "AVIF" => ImageFormat::Avif,
        _ => bail!("不支援此圖片格式。"),
    };
    let (mut pixels, (source_width, source_height)) = if format == ImageFormat::Avif {
        decode_avif(source)?
    } else {
        decode_pixels(source, format)?
    };
    if pixels.width().max(pixels.height()) > MAX_OUTPUT_SIDE {
        pixels = pixels.resize(MAX_OUTPUT_SIDE, MAX_OUTPUT_SIDE, FilterType::Lanczos3);
    }
    let pixels = if pixels.color().has_alpha() {
        DynamicImage::ImageRgba8(pixels.into_rgba8())
    } else {
        DynamicImage::ImageRgb8(pixels.into_rgb8())
    };
    let mut pixels = pixels;
    let bytes = loop {
        let mut output = PngOutput::default();
        let encoded = PngEncoder::new(&mut output).write_image(
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            pixels.color().into(),
        );
        if !output.exceeded {
            encoded?;
            break output.bytes;
        }
        ensure!(
            pixels.width().max(pixels.height()) > 1,
            "圖片無法縮小至傳輸限制內。"
        );
        pixels = pixels.resize_exact(
            (pixels.width() * 3 / 4).max(1),
            (pixels.height() * 3 / 4).max(1),
            FilterType::Lanczos3,
        );
    };
    Ok(json!({
        "metadata": {
            "source_format": expected, "source_width": source_width, "source_height": source_height,
            "width": pixels.width(), "height": pixels.height(),
            "source_bytes": source.len(), "image_bytes": bytes.len(),
            "resized": (source_width, source_height) != (pixels.width(), pixels.height()), "metadata_stripped": true
        },
        "png": STANDARD.encode(bytes)
    }))
}

#[cfg(test)]
mod tests {
    use super::avif_frame_to_image;
    use avif_rust::{
        DecodedFrame,
        av1::{ColorConfig, ColorDescription, ColorRange, FrameBuffers, PlaneBuffer, PlaneLayout},
    };

    #[test]
    fn avif_uses_encoded_rgb_and_the_reference_unspecified_matrix() {
        let planes = [80, 120, 170]
            .into_iter()
            .enumerate()
            .map(|(index, sample)| PlaneBuffer {
                layout: PlaneLayout {
                    plane: index as u8,
                    width: 1,
                    height: 1,
                    subsampling_x: 0,
                    subsampling_y: 0,
                    sample_count: 1,
                },
                samples: vec![sample],
            })
            .collect();
        let frame = DecodedFrame {
            width: 1,
            height: 1,
            render_width: 1,
            render_height: 1,
            bit_depth: 8,
            color_config: ColorConfig {
                high_bitdepth: false,
                twelve_bit: false,
                bit_depth: 8,
                monochrome: false,
                color_description: Some(ColorDescription {
                    color_primaries: 2,
                    transfer_characteristics: 16,
                    matrix_coefficients: 2,
                }),
                color_range: ColorRange::Full,
                subsampling_x: false,
                subsampling_y: false,
                chroma_sample_position: None,
                separate_uv_delta_q: false,
            },
            color_information: None,
            alpha_premultiplied: false,
            buffers: FrameBuffers {
                width: 1,
                height: 1,
                planes,
            },
        };
        let (image, dimensions) = avif_frame_to_image(frame).unwrap();
        assert_eq!(dimensions, (1, 1));
        for (actual, expected) in image
            .into_rgb8()
            .get_pixel(0, 0)
            .0
            .into_iter()
            .zip([139, 53, 66])
        {
            assert!(actual.abs_diff(expected) <= 1, "{actual} versus {expected}");
        }
    }
}
