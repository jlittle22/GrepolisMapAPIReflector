//! Renders a small PNG map of a world, zoomed to a set of towns and highlighting them, for
//! the map's "Copy town `BBCode`" to embed with `[img]`.

use std::collections::{HashMap, HashSet};

use crate::offset_data::OFFSET_DATA;

/// width and height of the image in pixels
const SIZE: usize = 320;
/// world units per ocean
const OCEAN: f32 = 100.0;
/// the view never zooms in closer than this many world units across
const MIN_SPAN: f32 = 40.0;

const BACKGROUND: [u8; 3] = [22, 34, 46];
const OCEAN_LINE: [u8; 3] = [44, 62, 80];
const OCEAN_LABEL: [u8; 3] = [92, 116, 138];
const OTHER_TOWN: [u8; 3] = [96, 110, 122];
const SELECTED_OUTLINE: [u8; 3] = [16, 24, 32];
/// the map's selection green, sampled from the Grass Touchers logo
const SELECTED: [u8; 3] = [143, 199, 62];

/// Render the towns of `towns_txt` (placed using `islands_txt`) zoomed to the towns in
/// `selected`, which are highlighted. `None` if none of `selected` exist.
pub fn render(towns_txt: &str, islands_txt: &str, selected: &HashSet<u32>) -> Option<Vec<u8>> {
    let positions = town_positions(towns_txt, islands_txt);
    let picked: Vec<(f32, f32)> = positions
        .iter()
        .filter(|(id, _)| selected.contains(id))
        .map(|&(_, pos)| pos)
        .collect();
    if picked.is_empty() {
        return None;
    }

    // a square view around the picked towns with some padding, kept inside the world
    let (min_x, max_x) = min_max(picked.iter().map(|p| p.0));
    let (min_y, max_y) = min_max(picked.iter().map(|p| p.1));
    let span = ((max_x - min_x).max(max_y - min_y) * 1.3).clamp(MIN_SPAN, 1000.0);
    let left = ((min_x + max_x - span) / 2.0).clamp(0.0, 1000.0 - span);
    let top = ((min_y + max_y - span) / 2.0).clamp(0.0, 1000.0 - span);
    #[allow(clippy::cast_precision_loss)]
    let scale = SIZE as f32 / span;
    let to_px = |x: f32, y: f32| ((x - left) * scale, (y - top) * scale);

    let mut image = Image::new(BACKGROUND);

    // ocean grid, then the towns, then ocean numbers (e.g. "44" for x 400..500, y 400..500)
    // so the numbers stay readable over busy oceans
    let mut labels = Vec::new();
    let mut ox = (left / OCEAN).floor();
    while ox * OCEAN < left + span {
        let mut oy = (top / OCEAN).floor();
        while oy * OCEAN < top + span {
            let (px, py) = to_px(ox * OCEAN, oy * OCEAN);
            image.vline(px, OCEAN_LINE);
            image.hline(py, OCEAN_LINE);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let number = (ox as u32) * 10 + oy as u32;
            labels.push((px.max(0.0) + 4.0, py.max(0.0) + 4.0, number.to_string()));
            oy += 1.0;
        }
        ox += 1.0;
    }

    let dot = (scale * 0.25).clamp(0.6, 2.0);
    for &(id, (x, y)) in &positions {
        if !selected.contains(&id) {
            let (px, py) = to_px(x, y);
            image.disc(px, py, dot, OTHER_TOWN);
        }
    }
    for (x, y, text) in &labels {
        image.text(*x, *y, text, OCEAN_LABEL);
    }
    let radius = (scale * 0.5).clamp(2.5, 5.0);
    for &(x, y) in &picked {
        let (px, py) = to_px(x, y);
        image.disc(px, py, radius + 1.2, SELECTED_OUTLINE);
        image.disc(px, py, radius, SELECTED);
    }

    image.border(OCEAN_LABEL);
    image.png()
}

fn min_max(values: impl Iterator<Item = f32>) -> (f32, f32) {
    values.fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// Every town's id and world position, computed like the map does: the island position plus
/// the slot's offset for that island type.
fn town_positions(towns_txt: &str, islands_txt: &str) -> Vec<(u32, (f32, f32))> {
    // (x, y) -> island type
    let island_types: HashMap<(u16, u16), u8> = islands_txt
        .lines()
        .filter_map(|line| {
            let mut values = line.split(',').skip(1);
            let x = values.next()?.parse().ok()?;
            let y = values.next()?.parse().ok()?;
            let typ = values.next()?.parse().ok()?;
            Some(((x, y), typ))
        })
        .collect();
    // (island type, slot) -> offset
    let offsets: HashMap<(u8, u8), (u16, u16)> = OFFSET_DATA
        .lines()
        .filter_map(|line| {
            let mut values = line.split(',');
            let typ = values.next()?.parse().ok()?;
            let x = values.next()?.parse().ok()?;
            let y = values.next()?.parse().ok()?;
            let slot = values.next()?.parse().ok()?;
            Some(((typ, slot), (x, y)))
        })
        .collect();

    towns_txt
        .lines()
        .filter_map(|line| {
            // id, player id, name, island x, island y, slot, points
            let mut values = line.split(',');
            let id = values.next()?.parse().ok()?;
            let mut values = values.skip(2);
            let x: u16 = values.next()?.parse().ok()?;
            let y: u16 = values.next()?.parse().ok()?;
            let slot: u8 = values.next()?.parse().ok()?;
            let (off_x, off_y) = island_types
                .get(&(x, y))
                .and_then(|typ| offsets.get(&(*typ, slot)))
                .map_or((0.5, 0.5), |&(ox, oy)| {
                    (f32::from(ox) / 125.0, f32::from(oy) / 125.0)
                });
            Some((id, (f32::from(x) + off_x, f32::from(y) + off_y)))
        })
        .collect()
}

/// a square RGB canvas of `SIZE` pixels
struct Image {
    pixels: Vec<u8>,
}

#[allow(clippy::cast_possible_wrap)] // SIZE is far below i64::MAX
impl Image {
    fn new(color: [u8; 3]) -> Self {
        Self {
            pixels: color.repeat(SIZE * SIZE),
        }
    }

    fn set(&mut self, x: i64, y: i64, color: [u8; 3]) {
        let Ok(x) = usize::try_from(x) else { return };
        let Ok(y) = usize::try_from(y) else { return };
        if x < SIZE && y < SIZE {
            let i = (y * SIZE + x) * 3;
            self.pixels[i..i + 3].copy_from_slice(&color);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn vline(&mut self, x: f32, color: [u8; 3]) {
        for y in 0..SIZE as i64 {
            self.set(x.round() as i64, y, color);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn hline(&mut self, y: f32, color: [u8; 3]) {
        for x in 0..SIZE as i64 {
            self.set(x, y.round() as i64, color);
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn disc(&mut self, cx: f32, cy: f32, radius: f32, color: [u8; 3]) {
        let r = radius.ceil() as i64;
        let (x0, y0) = (cx.floor() as i64, cy.floor() as i64);
        for y in y0 - r..=y0 + r {
            for x in x0 - r..=x0 + r {
                #[allow(clippy::cast_precision_loss)]
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                if dx * dx + dy * dy <= radius * radius {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// digits only, in a 3x5 pixel font drawn at twice the size
    #[allow(clippy::cast_possible_truncation)]
    fn text(&mut self, x: f32, y: f32, text: &str, color: [u8; 3]) {
        const DIGITS: [[u8; 5]; 10] = [
            [0b111, 0b101, 0b101, 0b101, 0b111],
            [0b010, 0b110, 0b010, 0b010, 0b111],
            [0b111, 0b001, 0b111, 0b100, 0b111],
            [0b111, 0b001, 0b111, 0b001, 0b111],
            [0b101, 0b101, 0b111, 0b001, 0b001],
            [0b111, 0b100, 0b111, 0b001, 0b111],
            [0b111, 0b100, 0b111, 0b101, 0b111],
            [0b111, 0b001, 0b001, 0b001, 0b001],
            [0b111, 0b101, 0b111, 0b101, 0b111],
            [0b111, 0b101, 0b111, 0b001, 0b111],
        ];
        let (mut x, y) = (x.round() as i64, y.round() as i64);
        for digit in text.chars().filter_map(|c| c.to_digit(10)) {
            for (row, bits) in DIGITS[digit as usize].iter().enumerate() {
                for col in 0..3 {
                    if bits & (0b100 >> col) != 0 {
                        let (px, py) = (x + col * 2, y + row as i64 * 2);
                        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            self.set(px + dx, py + dy, color);
                        }
                    }
                }
            }
            x += 8;
        }
    }

    fn border(&mut self, color: [u8; 3]) {
        let last = SIZE as i64 - 1;
        for i in 0..=last {
            for (x, y) in [(i, 0), (i, last), (0, i), (last, i)] {
                self.set(x, y, color);
            }
        }
    }

    fn png(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let size = u32::try_from(SIZE).ok()?;
        let mut encoder = png::Encoder::new(&mut out, size, size);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&self.pixels).ok()?;
        writer.finish().ok()?;
        Some(out)
    }
}
