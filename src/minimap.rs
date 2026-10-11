//! Renders a small PNG map of a world, zoomed to a set of towns and highlighting them, for
//! the map's "Copy town `BBCode`" to embed with `[img]`.

use std::collections::{HashMap, HashSet};

use crate::canvas::{Canvas, Rgba};
use crate::offset_data::OFFSET_DATA;

/// width and height of the image in pixels
const SIZE: usize = 320;
/// world units per ocean
const OCEAN: f32 = 100.0;
/// the view never zooms in closer than this many world units across
const MIN_SPAN: f32 = 40.0;

const BACKGROUND: [u8; 3] = [22, 34, 46];
const OCEAN_LINE: Rgba = [44, 62, 80, 255];
const OCEAN_LABEL: Rgba = [92, 116, 138, 255];
const OTHER_TOWN: Rgba = [96, 110, 122, 255];
const SELECTED_OUTLINE: Rgba = [16, 24, 32, 255];
/// the map's selection green, sampled from the Grass Touchers logo
const SELECTED: Rgba = [143, 199, 62, 255];

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
    let size = SIZE as f32;
    let scale = size / span;
    let to_px = |x: f32, y: f32| ((x - left) * scale, (y - top) * scale);

    let mut image = Canvas::new(SIZE, SIZE, BACKGROUND);

    // ocean grid, then the towns, then ocean numbers (e.g. "44" for x 400..500, y 400..500)
    // so the numbers stay readable over busy oceans
    let mut labels = Vec::new();
    let mut ox = (left / OCEAN).floor();
    while ox * OCEAN < left + span {
        let mut oy = (top / OCEAN).floor();
        while oy * OCEAN < top + span {
            let (px, py) = to_px(ox * OCEAN, oy * OCEAN);
            image.rect(px - 0.5, 0.0, px + 0.5, size, OCEAN_LINE);
            image.rect(0.0, py - 0.5, size, py + 0.5, OCEAN_LINE);
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
        image.digits(*x, *y, text, OCEAN_LABEL);
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
