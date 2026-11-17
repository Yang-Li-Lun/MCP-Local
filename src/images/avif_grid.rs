use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use avif_rust::{
    DecodedFrame,
    av1::{ColorRange, PlaneBuffer, PlaneLayout},
};
use gamut_isobmff::{ImageGrid, IsoBmffImage, Item, Property, PropertyKind};
use image::DynamicImage;

use super::{
    AVIF_DECODE_MEMORY, MAX_PIXELS, MAX_SOURCE, avif_frame_to_image, avif_orientation,
    native_avif_limits, supported_dimensions,
};
use crate::{media::isobmff_boxes as boxes, parser::MAX_MEMORY};

const INVALID: &str = "AVIF 格狀影像結構無效或不支援。";
const LIMIT: &str = "AVIF 格狀影像超出資源限制。";
const MAX_CELLS: usize = 64;
const MAX_METADATA: usize = 4 * 1024 * 1024;
const MAX_PROPERTY_COPIES: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 4096;
const ALPHA_TYPE: &str = "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";

pub(super) fn decode(source: &[u8]) -> Result<Option<(DynamicImage, (u32, u32))>> {
    let Some((container, parse_memory)) = parse(source).context("AVIF 格狀容器檢查失敗。")?
    else {
        return Ok(None);
    };
    let mut items = BTreeMap::new();
    let mut derived_inputs = BTreeSet::new();
    for item in &container.items {
        ensure!(items.insert(item.id, item).is_none(), "{INVALID}");
        for reference in item
            .references
            .iter()
            .filter(|r| &r.reference_type == b"dimg")
        {
            for &id in &reference.to_item_ids {
                ensure!(derived_inputs.insert(id), "{INVALID}");
            }
        }
    }
    let primary = items
        .get(&container.primary_item_id)
        .copied()
        .context(INVALID)?;
    ensure!(
        &primary.item_type == b"grid" && !primary.hidden,
        "{INVALID}"
    );
    let orientation = grid_orientation(primary)?;
    let color = Grid::new(primary, &items).context("AVIF 主影像分塊無效。")?;

    let mut alpha = None;
    let mut tile_alpha = BTreeMap::new();
    for item in &container.items {
        if item.properties.iter().any(|p| {
            matches!(&p.kind, PropertyKind::AuxiliaryType { aux_type, .. } if aux_type == ALPHA_TYPE)
        }) {
            for reference in item.references.iter().filter(|r| &r.reference_type == b"auxl") {
                if reference.to_item_ids.contains(&primary.id) {
                    ensure!(
                        reference.to_item_ids == [primary.id] && alpha.is_none(),
                        "{INVALID}"
                    );
                    alpha = Some(Grid::new(item, &items).context("AVIF 透明度分塊無效。")?);
                }
                for &id in &reference.to_item_ids {
                    if color.tiles.iter().any(|tile| tile.id == id) {
                        ensure!(
                            reference.to_item_ids == [id]
                                && tile_alpha.insert(id, item).is_none()
                                && !derived_inputs.contains(&item.id),
                            "{INVALID}"
                        );
                    }
                }
            }
        }
    }
    let mut premultiplied = false;
    for reference in primary
        .references
        .iter()
        .filter(|r| &r.reference_type == b"prem")
    {
        ensure!(
            !premultiplied
                && alpha
                    .as_ref()
                    .is_some_and(|a| reference.to_item_ids == [a.item.id]),
            "{INVALID}"
        );
        premultiplied = true;
    }
    if let Some(alpha) = &alpha {
        ensure!(alpha.output == color.output, "{INVALID}");
    }
    if alpha.is_none()
        && let Some(tiles) = color
            .tiles
            .iter()
            .map(|tile| tile_alpha.get(&tile.id).copied())
            .collect::<Option<Vec<_>>>()
    {
        for tile in &tiles {
            ensure!(
                &tile.item_type == b"av01" && ispe(tile)? == color.tile_size,
                "{INVALID}"
            );
        }
        alpha = Some(Grid {
            item: primary,
            output: color.output,
            tile_size: color.tile_size,
            columns: color.columns,
            tiles,
        });
    }
    let compressed_work = color
        .tiles
        .iter()
        .chain(alpha.iter().flat_map(|a| &a.tiles))
        .map(|tile| tile.payload.len() as u64)
        .sum::<u64>();
    ensure!(compressed_work <= (2 * MAX_SOURCE) as u64, "{LIMIT}");

    let mut frame = color
        .decode(parse_memory, false)
        .context("AVIF 主影像解碼失敗。")?;
    if let Some(alpha) = alpha {
        let mut alpha_frame = alpha
            .decode(parse_memory + frame_bytes(&frame), true)
            .context("AVIF 透明度解碼失敗。")?;
        let mut plane = alpha_frame.buffers.planes.pop().context(INVALID)?;
        plane.layout.plane = 3;
        if alpha_frame.bit_depth != frame.bit_depth {
            let from = (1_u32 << alpha_frame.bit_depth) - 1;
            let to = (1_u32 << frame.bit_depth) - 1;
            for sample in &mut plane.samples {
                *sample = ((u32::from(*sample) * to + from / 2) / from) as u16;
            }
        }
        frame.buffers.planes.push(plane);
    }
    frame.alpha_premultiplied = premultiplied;
    // Reserve both the temporary RGBA16 and final RGBA8 buffers used by color conversion.
    let conversion_bytes = frame.width * frame.height * 12;
    ensure!(
        frame_bytes(&frame) + conversion_bytes <= MAX_MEMORY - 2 * MAX_SOURCE,
        "{LIMIT}"
    );
    drop(container);
    let (mut pixels, dimensions) = avif_frame_to_image(frame)?;
    pixels.apply_orientation(orientation);
    Ok(Some((pixels, dimensions)))
}

fn parse(source: &[u8]) -> Result<Option<(IsoBmffImage, usize)>> {
    let mut count = 0;
    let top = boxes(source, &mut count)?;
    let mut metadata = None;
    let mut primary_grid = false;
    // Identify the primary item before handing any derived input to the native still decoder.
    for (_, body) in top.iter().filter(|(kind, _)| *kind == b"meta") {
        let entries = boxes(body.get(4..).context(INVALID)?, &mut count)?;
        let mut grid_ids = BTreeSet::new();
        for (_, iinf) in entries.iter().filter(|(kind, _)| *kind == b"iinf") {
            let header = match iinf.first() {
                Some(0) => 6,
                Some(1) => 8,
                _ => bail!(INVALID),
            };
            for (kind, infe) in boxes(iinf.get(header..).context(INVALID)?, &mut count)? {
                if kind == b"infe" {
                    match infe {
                        [2, _, _, _, a, b, _, _, b'g', b'r', b'i', b'd', ..] => {
                            grid_ids.insert(u32::from(u16::from_be_bytes([*a, *b])));
                        }
                        [3, _, _, _, a, b, c, d, _, _, b'g', b'r', b'i', b'd', ..] => {
                            grid_ids.insert(u32::from_be_bytes([*a, *b, *c, *d]));
                        }
                        _ => {}
                    }
                }
            }
        }
        for (_, pitm) in entries.iter().filter(|(kind, _)| *kind == b"pitm") {
            let id = match pitm {
                [0, _, _, _, a, b, ..] => u32::from(u16::from_be_bytes([*a, *b])),
                [1, _, _, _, a, b, c, d, ..] => u32::from_be_bytes([*a, *b, *c, *d]),
                _ => bail!(INVALID),
            };
            primary_grid |= grid_ids.contains(&id);
        }
        metadata = Some(entries);
    }
    if !primary_grid {
        if let Some(entries) = metadata {
            for (_, iprp) in entries.iter().filter(|(kind, _)| *kind == b"iprp") {
                let parts = boxes(iprp, &mut count)?;
                for (_, ipco) in parts.iter().filter(|(kind, _)| *kind == b"ipco") {
                    let properties = boxes(ipco, &mut count)?;
                    if properties.iter().any(|(kind, _)| requires_essential(kind)) {
                        for (_, ipma) in parts.iter().filter(|(kind, _)| *kind == b"ipma") {
                            check_property_associations(ipma, &properties, false)?;
                        }
                    }
                }
            }
        }
        return Ok(None);
    }
    let brands = one_box(&top, b"ftyp")?;
    let major = brands.get(..4).context(INVALID)?;
    let compatible = brands.get(8..).context(INVALID)?.chunks_exact(4);
    ensure!(
        brands.len() <= 4096
            && compatible.remainder().is_empty()
            && std::iter::once(major)
                .chain(compatible.clone())
                .any(|b| b == b"avif")
            && !top
                .iter()
                .any(|(kind, _)| matches!(*kind, b"moov" | b"trak"))
            && !std::iter::once(major)
                .chain(compatible)
                .any(|b| b == b"avis"),
        "{INVALID}"
    );
    ensure!(
        one_box(&top, b"meta")?.get(..4) == Some(&[0, 0, 0, 0]),
        "{INVALID}"
    );
    let entries = metadata.context(INVALID)?;
    for kind in [b"hdlr", b"pitm", b"iinf", b"iloc", b"iprp", b"iref"] {
        one_box(&entries, kind)?;
    }
    for kind in [b"grpl", b"idat"] {
        ensure!(
            entries.iter().filter(|(other, _)| *other == kind).count() <= 1,
            "{INVALID}"
        );
    }
    let metadata_bytes = entries
        .iter()
        .filter(|(kind, _)| *kind != b"idat")
        .map(|(_, body)| body.len() + 8)
        .sum::<usize>();
    ensure!(metadata_bytes <= MAX_METADATA, "{LIMIT}");
    let iloc = one_box(&entries, b"iloc")?;
    let iprp = boxes(one_box(&entries, b"iprp")?, &mut count)?;
    let properties = boxes(one_box(&iprp, b"ipco")?, &mut count)?;
    ensure!(properties.len() <= MAX_ITEMS, "{LIMIT}");
    let copies = check_property_associations(one_box(&iprp, b"ipma")?, &properties, true)?;
    let idat = entries
        .iter()
        .find_map(|(kind, body)| (*kind == b"idat").then_some(*body));
    let (mut locations, payload_bytes) = item_locations(iloc, source, idat)?;

    // Read the structural properties separately. Compressed items may share a
    // physical extent; account for every reference before materializing a copy.
    let mut metadata = Vec::new();
    write_box(&mut metadata, b"ftyp", brands);
    let mut meta = vec![0; 4];
    for (kind, body) in entries {
        if !matches!(kind, b"iloc" | b"idat") {
            write_box(&mut meta, kind, body);
        }
    }
    write_box(&mut metadata, b"meta", &meta);
    let mut container = gamut_isobmff::read(&metadata)?;
    for item in &mut container.items {
        if let Some(extents) = locations.remove(&item.id) {
            let size = extents.iter().map(|extent| extent.len()).sum();
            item.payload.try_reserve_exact(size)?;
            for extent in extents {
                item.payload.extend_from_slice(extent);
            }
        }
    }
    ensure!(locations.is_empty(), "{INVALID}");
    let retained = source.len() + payload_bytes + 16 * metadata_bytes + copies;
    Ok(Some((container, retained)))
}

fn write_box(bytes: &mut Vec<u8>, kind: &[u8], body: &[u8]) {
    bytes.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
}

type ItemLocations<'a> = BTreeMap<u32, Vec<&'a [u8]>>;

fn item_locations<'a>(
    mut iloc: &[u8],
    file: &'a [u8],
    idat: Option<&'a [u8]>,
) -> Result<(ItemLocations<'a>, usize)> {
    let [version @ 0..=2, 0, 0, 0, sizes, base_and_index, body @ ..] = iloc else {
        bail!(INVALID);
    };
    let version = *version;
    let [offset_width, length_width, base_width, index_width] = [
        sizes >> 4,
        sizes & 15,
        base_and_index >> 4,
        base_and_index & 15,
    ]
    .map(usize::from);
    ensure!(
        matches!(length_width, 4 | 8)
            && [offset_width, base_width, index_width]
                .iter()
                .all(|v| matches!(v, 0 | 4 | 8))
            && (version != 0 || index_width == 0),
        "{INVALID}"
    );
    iloc = body;
    let id_width = if version == 2 { 4 } else { 2 };
    let count = take_number(&mut iloc, id_width)? as usize;
    ensure!(count <= MAX_ITEMS, "{LIMIT}");
    let mut locations = BTreeMap::new();
    let mut extent_count = 0;
    let mut payload_bytes = 0;
    for _ in 0..count {
        let id = take_number(&mut iloc, id_width)? as u32;
        let method = if version == 0 {
            0
        } else {
            take_number(&mut iloc, 2)?
        };
        let data = match method {
            0 => file,
            1 => idat.context(INVALID)?,
            _ => bail!(INVALID),
        };
        ensure!(take_number(&mut iloc, 2)? == 0 && id != 0, "{INVALID}");
        let base = take_number(&mut iloc, base_width)?;
        let extents = take_number(&mut iloc, 2)? as usize;
        extent_count += extents;
        ensure!(extent_count <= MAX_ITEMS, "{LIMIT}");
        let mut segments = Vec::with_capacity(extents);
        for _ in 0..extents {
            take_number(&mut iloc, index_width)?;
            let offset = take_number(&mut iloc, offset_width)?;
            let length = take_number(&mut iloc, length_width)?;
            let start = usize::try_from(base.checked_add(offset).context(INVALID)?)?;
            let end = start
                .checked_add(usize::try_from(length)?)
                .context(INVALID)?;
            let segment = data.get(start..end).context(INVALID)?;
            payload_bytes += segment.len();
            ensure!(payload_bytes <= 2 * MAX_SOURCE, "{LIMIT}");
            segments.push(segment);
        }
        ensure!(locations.insert(id, segments).is_none(), "{INVALID}");
    }
    ensure!(iloc.is_empty(), "{INVALID}");
    Ok((locations, payload_bytes))
}

fn take_number(data: &mut &[u8], width: usize) -> Result<u64> {
    let (head, tail) = data.split_at_checked(width).context(INVALID)?;
    let mut value = [0; 8];
    value[8 - width..].copy_from_slice(head);
    *data = tail;
    Ok(u64::from_be_bytes(value))
}

fn one_box<'a>(boxes: &[(&[u8], &'a [u8])], kind: &[u8; 4]) -> Result<&'a [u8]> {
    let mut found = boxes.iter().filter(|(other, _)| *other == kind);
    let body = found.next().context(INVALID)?.1;
    ensure!(found.next().is_none(), "{INVALID}");
    Ok(body)
}

fn requires_essential(kind: &[u8]) -> bool {
    matches!(kind, b"clap" | b"irot" | b"imir")
}

fn check_property_associations(
    mut data: &[u8],
    properties: &[(&[u8], &[u8])],
    charge: bool,
) -> Result<usize> {
    let [version, _, _, flags, rows @ ..] = data else {
        bail!(INVALID);
    };
    let id_width = match version {
        0 => 2,
        1 => 4,
        _ => bail!(INVALID),
    };
    let wide = flags & 1 != 0;
    let (entries, rest) = rows.split_first_chunk::<4>().context(INVALID)?;
    let entries = u32::from_be_bytes(*entries) as usize;
    ensure!(entries <= MAX_ITEMS, "{LIMIT}");
    data = rest;
    let mut copies = 0;
    let mut associations = 0;
    for _ in 0..entries {
        let (&count, rest) = data
            .get(id_width..)
            .and_then(|d| d.split_first())
            .context(INVALID)?;
        data = rest;
        associations += usize::from(count);
        ensure!(associations <= MAX_ITEMS, "{LIMIT}");
        for _ in 0..count {
            let (index, essential) = if wide {
                let (word, tail) = data.split_first_chunk::<2>().context(INVALID)?;
                data = tail;
                let word = u16::from_be_bytes(*word);
                (usize::from(word & 0x7fff), word & 0x8000 != 0)
            } else {
                let (&byte, tail) = data.split_first().context(INVALID)?;
                data = tail;
                (usize::from(byte & 0x7f), byte & 0x80 != 0)
            };
            if index == 0 && !essential && !charge {
                continue;
            }
            let (kind, body) = properties
                .get(index.checked_sub(1).context(INVALID)?)
                .context(INVALID)?;
            ensure!(essential || !requires_essential(kind), "{INVALID}");
            if charge {
                copies += 128 + body.len();
                ensure!(copies <= MAX_PROPERTY_COPIES, "{LIMIT}");
            }
        }
    }
    ensure!(data.is_empty(), "{INVALID}");
    Ok(copies)
}

struct Grid<'a> {
    item: &'a Item,
    output: (u32, u32),
    tile_size: (u32, u32),
    columns: usize,
    tiles: Vec<&'a Item>,
}

impl<'a> Grid<'a> {
    fn new(item: &'a Item, items: &BTreeMap<u32, &'a Item>) -> Result<Self> {
        let output = ispe(item)?;
        supported_dimensions(output.0, output.1)?;
        if &item.item_type == b"av01" {
            return Ok(Self {
                item,
                output,
                tile_size: output,
                columns: 1,
                tiles: vec![item],
            });
        }
        ensure!(
            &item.item_type == b"grid" && item.payload.get(1).is_some_and(|f| f & !1 == 0),
            "{INVALID}"
        );
        let layout = ImageGrid::parse(&item.payload)?;
        ensure!(
            output == (layout.output_width, layout.output_height),
            "{INVALID}"
        );
        let columns = usize::from(layout.columns);
        let rows = usize::from(layout.rows);
        let count = rows * columns;
        ensure!(count <= MAX_CELLS, "{LIMIT}");
        let mut references = item
            .references
            .iter()
            .filter(|r| &r.reference_type == b"dimg");
        let ids = &references.next().context(INVALID)?.to_item_ids;
        ensure!(
            ids.len() == count && references.next().is_none(),
            "{INVALID}"
        );
        let tiles = ids
            .iter()
            .map(|id| items.get(id).copied().context(INVALID))
            .collect::<Result<Vec<_>>>()?;
        let tile_size = ispe(tiles[0])?;
        supported_dimensions(tile_size.0, tile_size.1)?;
        ensure!(
            (count == 1 || (tile_size.0 >= 64 && tile_size.1 >= 64))
                && (u64::from(tile_size.0) * u64::from(tile_size.1) * count as u64) <= MAX_PIXELS
                && (u64::from(output.0).div_ceil(u64::from(tile_size.0))) == columns as u64
                && (u64::from(output.1).div_ceil(u64::from(tile_size.1))) == rows as u64,
            "{LIMIT}"
        );
        for tile in &tiles {
            ensure!(
                &tile.item_type == b"av01" && ispe(tile)? == tile_size,
                "{INVALID}"
            );
        }
        Ok(Self {
            item,
            output,
            tile_size,
            columns,
            tiles,
        })
    }

    fn decode(&self, retained: usize, alpha: bool) -> Result<DecodedFrame> {
        let mut canvas: Option<DecodedFrame> = None;
        let mut retained = retained;
        for (index, item) in self.tiles.iter().enumerate() {
            let bytes = standalone_tile(item, self.item, alpha)?;
            ensure!(bytes.len() <= MAX_SOURCE, "{LIMIT}");
            let budget = AVIF_DECODE_MEMORY
                .checked_sub(retained + 3 * bytes.len())
                .context(LIMIT)?;
            let cell = avif_rust::decode_frame_bytes_strict_with_limits(
                &bytes,
                &native_avif_limits(budget)?,
            )
            .with_context(|| format!("AVIF 分塊 {index} 解碼失敗。"))?
            .into_frame();
            drop(bytes);
            ensure!(
                (cell.width, cell.height) == (self.tile_size.0 as usize, self.tile_size.1 as usize)
                    && matches!(cell.bit_depth, 8 | 10 | 12)
                    && (!alpha
                        || (cell.color_config.monochrome
                            && cell.color_config.color_range == ColorRange::Full
                            && cell.buffers.planes.len() == 1)),
                "{INVALID}"
            );
            if self.tiles.len() == 1 && self.output == self.tile_size {
                return Ok(cell);
            }
            if canvas.is_none() {
                let width = self.output.0 as usize;
                let height = self.output.1 as usize;
                let mut plans = Vec::with_capacity(cell.buffers.planes.len());
                for plane in &cell.buffers.planes {
                    let mut layout = plane.layout;
                    layout.width = width.div_ceil(1 << layout.subsampling_x);
                    layout.height = height.div_ceil(1 << layout.subsampling_y);
                    layout.sample_count = layout.width * layout.height;
                    plans.push(layout);
                }
                retained += plans
                    .iter()
                    .map(|layout| 2 * layout.sample_count)
                    .sum::<usize>();
                ensure!(
                    retained + frame_bytes(&cell) < AVIF_DECODE_MEMORY,
                    "{LIMIT}"
                );
                let mut planes = Vec::with_capacity(plans.len());
                for layout in plans {
                    let mut samples = Vec::new();
                    samples.try_reserve_exact(layout.sample_count)?;
                    samples.resize(layout.sample_count, 0);
                    planes.push(PlaneBuffer { layout, samples });
                }
                canvas = Some(DecodedFrame {
                    width,
                    height,
                    render_width: width,
                    render_height: height,
                    bit_depth: cell.bit_depth,
                    color_config: cell.color_config,
                    color_information: cell.color_information.clone(),
                    alpha_premultiplied: false,
                    buffers: avif_rust::av1::FrameBuffers {
                        width,
                        height,
                        planes,
                    },
                });
            }
            let frame = canvas.as_mut().context(INVALID)?;
            ensure!(
                frame.color_config == cell.color_config
                    && frame.color_information == cell.color_information
                    && frame.buffers.planes.len() == cell.buffers.planes.len(),
                "{INVALID}"
            );
            let x = index % self.columns * cell.width;
            let y = index / self.columns * cell.height;
            for (dst, src) in frame.buffers.planes.iter_mut().zip(&cell.buffers.planes) {
                copy_plane(dst, src, x, y)?;
            }
        }
        canvas.context(INVALID)
    }
}

fn ispe(item: &Item) -> Result<(u32, u32)> {
    let mut extents = item.properties.iter().filter_map(|p| match p.kind {
        PropertyKind::ImageSpatialExtents { width, height } => Some((width, height)),
        _ => None,
    });
    let size = extents.next().context(INVALID)?;
    ensure!(extents.next().is_none(), "{INVALID}");
    Ok(size)
}

fn standalone_tile(item: &Item, master: &Item, alpha: bool) -> Result<Vec<u8>> {
    let color_override = !alpha
        && master
            .properties
            .iter()
            .any(|p| matches!(p.kind, PropertyKind::Colour(_)));
    let mut properties = Vec::new();
    for property in &item.properties {
        match &property.kind {
            PropertyKind::CodecConfiguration { kind, .. } if kind == b"av1C" => {
                properties.push(property.clone())
            }
            PropertyKind::ImageSpatialExtents { .. } | PropertyKind::PixelInformation { .. } => {
                properties.push(property.clone())
            }
            PropertyKind::Colour(_) if !alpha && !color_override => {
                properties.push(property.clone())
            }
            PropertyKind::AuxiliaryType { .. } if alpha => {}
            PropertyKind::Rotation(_)
            | PropertyKind::Mirror(_)
            | PropertyKind::CleanAperture { .. } => bail!(INVALID),
            _ if property.essential => bail!(INVALID),
            _ => {}
        }
    }
    if color_override {
        properties.extend(
            master
                .properties
                .iter()
                .filter(|p| matches!(p.kind, PropertyKind::Colour(_)))
                .cloned(),
        );
    }
    let tile = Item {
        id: 1,
        item_type: *b"av01",
        name: String::new(),
        hidden: false,
        content_type: None,
        content_encoding: None,
        references: Vec::new(),
        properties,
        payload: item.payload.clone(),
    };
    Ok(gamut_isobmff::write(&IsoBmffImage {
        major_brand: *b"avif",
        minor_version: 0,
        compatible_brands: vec![*b"avif", *b"mif1", *b"miaf"],
        primary_item_id: tile.id,
        items: vec![tile],
        groups: Vec::new(),
    })?)
}

fn frame_bytes(frame: &DecodedFrame) -> usize {
    let planes = frame
        .buffers
        .planes
        .iter()
        .map(|plane| 2 * plane.samples.len())
        .sum::<usize>();
    planes
        + frame
            .color_information
            .as_ref()
            .map_or(0, |color| color.payload.len())
}

fn copy_plane(dst: &mut PlaneBuffer, src: &PlaneBuffer, x: usize, y: usize) -> Result<()> {
    let PlaneLayout {
        subsampling_x,
        subsampling_y,
        ..
    } = src.layout;
    let sx = 1 << subsampling_x;
    let sy = 1 << subsampling_y;
    ensure!(
        dst.layout.plane == src.layout.plane
            && dst.layout.subsampling_x == subsampling_x
            && dst.layout.subsampling_y == subsampling_y
            && x % sx == 0
            && y % sy == 0,
        "{INVALID}"
    );
    let x = x / sx;
    let y = y / sy;
    ensure!(x < dst.layout.width && y < dst.layout.height, "{INVALID}");
    let w = src.layout.width.min(dst.layout.width - x);
    let h = src.layout.height.min(dst.layout.height - y);
    for (row, values) in src
        .samples
        .chunks_exact(src.layout.width)
        .take(h)
        .enumerate()
    {
        let start = (y + row) * dst.layout.width + x;
        dst.samples[start..start + w].copy_from_slice(&values[..w]);
    }
    Ok(())
}

fn grid_orientation(item: &Item) -> Result<image::metadata::Orientation> {
    let (width, height) = ispe(item)?;
    let mut rotation = None;
    let mut mirror = None;
    let mut aperture = false;
    for Property { kind, essential } in &item.properties {
        match *kind {
            PropertyKind::Rotation(angle) => {
                ensure!(rotation.replace(angle).is_none(), "{INVALID}")
            }
            PropertyKind::Mirror(axis) => ensure!(mirror.replace(axis).is_none(), "{INVALID}"),
            PropertyKind::CleanAperture {
                width_n,
                width_d,
                height_n,
                height_d,
                horiz_off_n,
                horiz_off_d,
                vert_off_n,
                vert_off_d,
            } => {
                ensure!(!aperture, "{INVALID}");
                // The reference decoder exposes the full raster after validating clap.
                validate_crop_axis(width, width_n, width_d, horiz_off_n, horiz_off_d)?;
                validate_crop_axis(height, height_n, height_d, vert_off_n, vert_off_d)?;
                aperture = true;
            }
            PropertyKind::ImageSpatialExtents { .. }
            | PropertyKind::Colour(_)
            | PropertyKind::PixelInformation { .. } => (),
            _ if *essential => bail!(INVALID),
            _ => (),
        }
    }
    avif_orientation(rotation.unwrap_or(0), mirror)
}

fn validate_crop_axis(side: u32, n: u32, d: u32, offset_n: u32, offset_d: u32) -> Result<()> {
    ensure!(d != 0 && offset_d != 0 && n % d == 0, "{INVALID}");
    let crop = n / d;
    ensure!(crop > 0 && crop <= side, "{INVALID}");
    let denominator = 2 * i64::from(offset_d);
    let start =
        (i64::from(side) - i64::from(crop)) * i64::from(offset_d) + 2 * i64::from(offset_n as i32);
    ensure!(
        start >= 0 && start % denominator == 0 && start / denominator <= i64::from(side - crop),
        "{INVALID}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check_property_associations, item_locations};

    #[test]
    fn item_locations_charge_shared_bytes_and_validate_bounds() {
        let mut iloc = vec![1, 0, 0, 0, 0x44, 0, 0, 2];
        for id in [1_u16, 2] {
            iloc.extend_from_slice(&id.to_be_bytes());
            iloc.extend_from_slice(b"\0\x01\0\0\0\x01");
            iloc.extend_from_slice(&3_u32.to_be_bytes());
            iloc.extend_from_slice(&4_u32.to_be_bytes());
        }
        let raw = b"....";
        let idat = b"---data";
        let (locations, charged) = item_locations(&iloc, raw, Some(idat)).unwrap();
        assert_eq!(locations[&1], [b"data".as_slice()]);
        assert_eq!(locations[&2], [b"data".as_slice()]);
        assert_eq!(charged, 8);
        assert!(item_locations(&iloc, raw, Some(b"---dat")).is_err());
        assert!(item_locations(&iloc, raw, None).is_err());
        iloc[12] = 1;
        assert!(item_locations(&iloc, raw, Some(idat)).is_err());
    }

    #[test]
    fn rejects_nonessential_geometry_associations() {
        let props = [(b"irot".as_slice(), &[1][..])];
        let mut ipma = [0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 1, 1];
        assert!(check_property_associations(&ipma, &props, false).is_err());
        ipma[11] |= 0x80;
        assert!(check_property_associations(&ipma, &props, false).is_ok());
    }
}
