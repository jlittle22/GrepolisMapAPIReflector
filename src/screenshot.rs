//! Renders the map's "Copy screenshot link" scenes: a plain text list of what the map drew
//! in its viewport, in screen pixels, so the image matches whatever filters, selections and
//! history snapshot the map was showing. For example:
//!
//! ```text
//! gtmap-screenshot 1
//! size 1280 720 1b1b1b
//! line 0 412.5 1280 412.5 2 3c3c3cff
//! line 0 80 1280 80 1 3c3c3cff 7 7
//! dots b4b4b4ff 1.6
//! 512.3 300.1
//! 515 302.8
//! rings 8fc73eff 3.6 2
//! 515 302.8
//! polys 5870801a 587080ff 1
//! 500 290 530 288 534 310 502 315
//! logo 8 8 46.7 48
//! ```
//!
//! `line` is axis-aligned, with an optional dash and gap length. `dots` and `rings` apply to
//! the `X Y` lines that follow them, `polys` (fill, outline color and width) to the lines of
//! `X Y X Y ...` corners that follow it, one convex polygon each. Colors are `RRGGBBAA`.

use lazy_static::lazy_static;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::canvas::{Canvas, Rgba, RgbaImage};

const HEADER: &str = "gtmap-screenshot 1";
/// largest width or height in pixels
pub const MAX_SIZE: usize = 2000;
/// most `dots`/`rings` points one scene may draw
const MAX_POINTS: usize = 200_000;
/// most lines one scene may draw
const MAX_LINES: usize = 1000;
/// most `polys` polygons one scene may draw
const MAX_POLYGONS: usize = 50_000;
/// most corners of one polygon
const MAX_CORNERS: usize = 64;

lazy_static! {
    static ref LOGO: Option<RgbaImage> =
        RgbaImage::decode_png(include_bytes!("../assets/grass_touchers_logo.png"));
}

/// The id a scene is stored under: the first 16 hex digits of its SHA-256, so the map can
/// work out the link before uploading.
pub fn id(scene: &[u8]) -> String {
    Sha256::digest(scene)[..8]
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// what the `X Y` lines after a `dots` or `rings` draw
enum Points {
    None,
    Dots(Rgba, f32),
    Rings(Rgba, f32, f32),
    /// fill, outline color and width
    Polygons(Rgba, Rgba, f32),
}

/// Render `scene` to a PNG. `Err` says what's wrong with it.
#[allow(clippy::many_single_char_names)]
pub fn render(scene: &str) -> Result<Vec<u8>, String> {
    let mut lines = scene.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("first line must be `{HEADER}`"));
    }
    let size = lines.next().ok_or("missing size")?;
    let mut canvas = match size.split(' ').collect::<Vec<_>>()[..] {
        ["size", w, h, background] => {
            let w: usize = w.parse().map_err(|_| "bad width")?;
            let h: usize = h.parse().map_err(|_| "bad height")?;
            if !(1..=MAX_SIZE).contains(&w) || !(1..=MAX_SIZE).contains(&h) {
                return Err(format!("size must be 1 to {MAX_SIZE} pixels"));
            }
            let [r, g, b, _] = color(&format!("{background}ff"))?;
            Canvas::new(w, h, [r, g, b])
        }
        _ => return Err("second line must be `size W H RRGGBB`".to_owned()),
    };

    let mut points = Points::None;
    let (mut point_count, mut line_count, mut polygon_count) = (0, 0, 0);

    for (number, line) in lines.enumerate() {
        let fail = |what: &str| format!("line {}: {what}", number + 3);
        let words: Vec<&str> = line.split(' ').collect();
        match words[..] {
            ["line", x1, y1, x2, y2, width, rgba, ref dash @ ..] => {
                line_count += 1;
                if line_count > MAX_LINES {
                    return Err(fail("too many lines"));
                }
                let [x1, y1, x2, y2, width] =
                    numbers([x1, y1, x2, y2, width]).ok_or_else(|| fail("bad line"))?;
                let dash = match dash {
                    [] => None,
                    [dash, gap] => Some(numbers([dash, gap]).ok_or_else(|| fail("bad dash"))?),
                    _ => return Err(fail("bad dash")),
                };
                let rgba = color(rgba).map_err(|e| fail(&e))?;
                axis_line(&mut canvas, (x1, y1), (x2, y2), width, rgba, dash);
            }
            ["dots", rgba, radius] => {
                let [radius] = numbers([radius]).ok_or_else(|| fail("bad radius"))?;
                points = Points::Dots(color(rgba).map_err(|e| fail(&e))?, radius.min(50.0));
            }
            ["rings", rgba, radius, width] => {
                let [radius, width] = numbers([radius, width]).ok_or_else(|| fail("bad ring"))?;
                points = Points::Rings(
                    color(rgba).map_err(|e| fail(&e))?,
                    radius.min(50.0),
                    width.min(20.0),
                );
            }
            ["polys", fill, stroke, width] => {
                let [width] = numbers([width]).ok_or_else(|| fail("bad polys"))?;
                points = Points::Polygons(
                    color(fill).map_err(|e| fail(&e))?,
                    color(stroke).map_err(|e| fail(&e))?,
                    width.min(20.0),
                );
            }
            ["logo", x, y, w, h] => {
                let [x, y, w, h] = numbers([x, y, w, h]).ok_or_else(|| fail("bad logo"))?;
                #[allow(clippy::cast_precision_loss)]
                let max = MAX_SIZE as f32;
                if let Some(logo) = LOGO.as_ref() {
                    canvas.image(logo, x, y, w.min(max), h.min(max));
                }
            }
            ref corners if matches!(points, Points::Polygons(..)) => {
                let Points::Polygons(fill, stroke, width) = points else {
                    unreachable!()
                };
                polygon_count += 1;
                if polygon_count > MAX_POLYGONS {
                    return Err(fail("too many polygons"));
                }
                let corner_points = corners_of(corners)
                    .ok_or_else(|| fail("a polygon needs 3 or more `X Y` corners"))?;
                canvas.polygon(&corner_points, fill, stroke, width);
            }
            [x, y] => {
                point_count += 1;
                if point_count > MAX_POINTS {
                    return Err(fail("too many points"));
                }
                let [x, y] = numbers([x, y]).ok_or_else(|| fail("bad point"))?;
                match points {
                    Points::None => return Err(fail("point before `dots` or `rings`")),
                    Points::Dots(rgba, radius) => canvas.disc(x, y, radius, rgba),
                    Points::Rings(rgba, radius, width) => canvas.ring(x, y, radius, width, rgba),
                    Points::Polygons(..) => unreachable!("handled above"),
                }
            }
            [""] => {}
            _ => return Err(fail("unknown command")),
        }
    }
    canvas.png().ok_or_else(|| "failed to encode".to_owned())
}

/// the corners of a polygon line, `None` unless it's 3 to `MAX_CORNERS` `X Y` pairs
#[allow(clippy::manual_is_multiple_of)] // is_multiple_of needs Rust 1.87, the image has 1.83
fn corners_of(words: &[&str]) -> Option<Vec<(f32, f32)>> {
    if words.len() < 6 || words.len() % 2 != 0 || words.len() > MAX_CORNERS * 2 {
        return None;
    }
    words
        .chunks(2)
        .map(|pair| numbers([pair[0], pair[1]]).map(|[x, y]| (x, y)))
        .collect()
}

/// parse finite numbers, `None` if any isn't one
fn numbers<const N: usize>(words: [&str; N]) -> Option<[f32; N]> {
    let mut out = [0.0; N];
    for (slot, word) in out.iter_mut().zip(words) {
        *slot = word.parse().ok().filter(|n: &f32| n.is_finite())?;
    }
    Some(out)
}

fn color(hex: &str) -> Result<Rgba, String> {
    let bad = || format!("bad color `{hex}`, expected RRGGBBAA");
    if hex.len() != 8 {
        return Err(bad());
    }
    let mut out = [0; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(i * 2..i * 2 + 2).ok_or_else(bad)?, 16)
            .map_err(|_| bad())?;
    }
    Ok(out)
}

/// A horizontal or vertical line, solid or dashed with dashes starting at `from`. Other
/// directions aren't drawn, the map never needs them.
fn axis_line(
    canvas: &mut Canvas,
    from: (f32, f32),
    to: (f32, f32),
    width: f32,
    rgba: Rgba,
    dash: Option<[f32; 2]>,
) {
    let half = width.clamp(0.0, 20.0) / 2.0;
    // the line as a run along one axis at a fixed position on the other
    let (horizontal, at, start, end) = if (from.1 - to.1).abs() < f32::EPSILON {
        (true, from.1, from.0, to.0)
    } else if (from.0 - to.0).abs() < f32::EPSILON {
        (false, from.0, from.1, to.1)
    } else {
        return;
    };
    let mut draw = |a: f32, b: f32| {
        if horizontal {
            canvas.rect(a, at - half, b, at + half, rgba);
        } else {
            canvas.rect(at - half, a, at + half, b, rgba);
        }
    };
    let (direction, length) = (if end >= start { 1.0 } else { -1.0 }, (end - start).abs());
    #[allow(clippy::cast_precision_loss)]
    let limit = MAX_SIZE as f32 + 1.0;
    match dash {
        Some([dash, gap]) if dash > 0.0 && gap >= 0.0 => {
            // skip the dashes before the canvas, the line can run far outside it
            let period = dash + gap;
            let visible_from = if direction > 0.0 {
                -1.0 - start
            } else {
                start - limit
            };
            let mut t = (visible_from / period).floor().max(0.0) * period;
            while t < length {
                let a = start + direction * t;
                let b = start + direction * (t + dash).min(length);
                if (direction > 0.0 && a > limit) || (direction < 0.0 && a < -1.0) {
                    break;
                }
                draw(a, b);
                t += period;
            }
        }
        _ => draw(start, end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_scene() {
        let scene = "gtmap-screenshot 1\nsize 64 48 101010\nline 0 10 64 10 2 404040ff\n\
                     line 20 0 20 48 1 404040ff 7 7\ndots b4b4b4ff 1.5\n5 5\n30.5 20\n\
                     rings 8fc73eff 3 2\n30.5 20\npolys 5870801a 587080ff 1\n\
                     40 10 60 12 58 30 42 28\nlogo 2 2 10 10\n";
        let png = render(scene).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn rejects_bad_scenes() {
        assert!(render("nope").is_err());
        assert!(render("gtmap-screenshot 1\nsize 5000 10 000000").is_err());
        assert!(render("gtmap-screenshot 1\nsize 10 10 000000\n1 2").is_err());
        assert!(render("gtmap-screenshot 1\nsize 10 10 000000\ndots red 2").is_err());
        let polys = "gtmap-screenshot 1\nsize 10 10 000000\npolys 000000ff 000000ff 1\n";
        assert!(render(&format!("{polys}1 2 3 4")).is_err());
        assert!(render(&format!("{polys}1 2 3 4 5 6 7")).is_err());
        assert!(render(&format!("{polys}1 2 3 4 5 6")).is_ok());
    }

    #[test]
    fn id_is_16_hex_digits() {
        assert_eq!(id(b"abc"), "ba7816bf8f01cfea");
    }
}
