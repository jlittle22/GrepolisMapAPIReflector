//! A tiny RGB raster with just the drawing the minimap and screenshots need.

/// an RGB color plus alpha, 255 being opaque
pub type Rgba = [u8; 4];

pub struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // canvases are at most a few thousand pixels across
impl Canvas {
    pub fn new(width: usize, height: usize, background: [u8; 3]) -> Self {
        Self {
            width,
            height,
            pixels: background.repeat(width * height),
        }
    }

    /// blend `color` over the pixel at `x`, `y` with `coverage` (0 to 1) of its alpha
    fn blend(&mut self, x: i64, y: i64, color: Rgba, coverage: f32) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x >= self.width || y >= self.height {
            return;
        }
        let alpha = coverage.clamp(0.0, 1.0) * f32::from(color[3]) / 255.0;
        if alpha <= 0.0 {
            return;
        }
        let i = (y * self.width + x) * 3;
        for (pixel, &target) in self.pixels[i..i + 3].iter_mut().zip(&color[..3]) {
            let old = f32::from(*pixel);
            *pixel = (old + (f32::from(target) - old) * alpha).round() as u8;
        }
    }

    /// fill the rectangle from `x0`, `y0` to `x1`, `y1`, with partial edge pixels blended
    pub fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Rgba) {
        let (x0, x1) = (x0.min(x1).max(0.0), x0.max(x1).min(self.width as f32));
        let (y0, y1) = (y0.min(y1).max(0.0), y0.max(y1).min(self.height as f32));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0.floor() as i64..y1.ceil() as i64 {
            let cover_y = (y1.min(y as f32 + 1.0) - y0.max(y as f32)).max(0.0);
            for x in x0.floor() as i64..x1.ceil() as i64 {
                let cover_x = (x1.min(x as f32 + 1.0) - x0.max(x as f32)).max(0.0);
                self.blend(x, y, color, cover_x * cover_y);
            }
        }
    }

    /// an anti-aliased filled circle
    pub fn disc(&mut self, cx: f32, cy: f32, radius: f32, color: Rgba) {
        let r = radius.ceil() as i64 + 1;
        let (x0, y0) = (cx.floor() as i64, cy.floor() as i64);
        for y in y0 - r..=y0 + r {
            for x in x0 - r..=x0 + r {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let distance = (dx * dx + dy * dy).sqrt();
                self.blend(x, y, color, radius + 0.5 - distance);
            }
        }
    }

    /// an anti-aliased circle outline of `width` centered on `radius`
    pub fn ring(&mut self, cx: f32, cy: f32, radius: f32, width: f32, color: Rgba) {
        let outer = radius + width / 2.0;
        let inner = radius - width / 2.0;
        let r = outer.ceil() as i64 + 1;
        let (x0, y0) = (cx.floor() as i64, cy.floor() as i64);
        for y in y0 - r..=y0 + r {
            for x in x0 - r..=x0 + r {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let distance = (dx * dx + dy * dy).sqrt();
                let coverage = (outer + 0.5 - distance).min(distance - inner + 0.5);
                self.blend(x, y, color, coverage);
            }
        }
    }

    /// A convex polygon, filled and outlined with `width` centered on its edges, both
    /// anti-aliased. Either winding works.
    pub fn polygon(&mut self, points: &[(f32, f32)], fill: Rgba, stroke: Rgba, width: f32) {
        if points.len() < 3 {
            return;
        }
        let area: f32 = (0..points.len())
            .map(|i| {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum();
        if area.abs() < f32::EPSILON {
            return;
        }
        let side = area.signum();
        // per edge: start point and inward unit normal, so the distance inside is a dot product
        let edges: Vec<((f32, f32), (f32, f32))> = (0..points.len())
            .filter_map(|i| {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let length = (dx * dx + dy * dy).sqrt();
                (length > f32::EPSILON).then(|| (a, (-dy * side / length, dx * side / length)))
            })
            .collect();
        let half = width.max(0.0) / 2.0;
        let reach = half + 1.0;
        let (min_x, max_x) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
            (lo.min(p.0), hi.max(p.0))
        });
        let (min_y, max_y) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
            (lo.min(p.1), hi.max(p.1))
        });
        let x0 = (min_x - reach).floor().max(0.0) as i64;
        let x1 = (max_x + reach).ceil().min(self.width as f32) as i64;
        let y0 = (min_y - reach).floor().max(0.0) as i64;
        let y1 = (max_y + reach).ceil().min(self.height as f32) as i64;
        for y in y0..y1 {
            for x in x0..x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // signed distance to the outline, positive inside (exact for convex shapes
                // except just outside the corners, where it's a little short)
                let distance = edges
                    .iter()
                    .map(|((ax, ay), (nx, ny))| (px - ax) * nx + (py - ay) * ny)
                    .fold(f32::MAX, f32::min);
                self.blend(x, y, fill, distance + 0.5);
                if half > 0.0 {
                    self.blend(x, y, stroke, half + 0.5 - distance.abs());
                }
            }
        }
    }

    /// digits only, in a 3x5 pixel font drawn at twice the size
    pub fn digits(&mut self, x: f32, y: f32, text: &str, color: Rgba) {
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
        let (mut x, y) = (x.round(), y.round());
        for digit in text.chars().filter_map(|c| c.to_digit(10)) {
            for (row, bits) in DIGITS[digit as usize].iter().enumerate() {
                for col in 0..3u8 {
                    if bits & (0b100 >> col) != 0 {
                        let (px, py) = (x + f32::from(col) * 2.0, y + row as f32 * 2.0);
                        self.rect(px, py, px + 2.0, py + 2.0, color);
                    }
                }
            }
            x += 8.0;
        }
    }

    /// a 1 pixel frame around the whole canvas
    pub fn border(&mut self, color: Rgba) {
        let (w, h) = (self.width as f32, self.height as f32);
        self.rect(0.0, 0.0, w, 1.0, color);
        self.rect(0.0, h - 1.0, w, h, color);
        self.rect(0.0, 0.0, 1.0, h, color);
        self.rect(w - 1.0, 0.0, w, h, color);
    }

    #[allow(clippy::many_single_char_names)]
    /// draw `image` scaled into the rectangle at `x`, `y` of `w` by `h`, sampling the nearest
    /// source pixel
    pub fn image(&mut self, image: &RgbaImage, x: f32, y: f32, w: f32, h: f32) {
        if image.width == 0 || image.height == 0 || w <= 0.0 || h <= 0.0 {
            return;
        }
        for py in y.floor() as i64..(y + h).ceil() as i64 {
            for px in x.floor() as i64..(x + w).ceil() as i64 {
                let u = ((px as f32 + 0.5 - x) / w * image.width as f32).floor();
                let v = ((py as f32 + 0.5 - y) / h * image.height as f32).floor();
                if u < 0.0 || v < 0.0 {
                    continue;
                }
                let (u, v) = (u as usize, v as usize);
                if u < image.width && v < image.height {
                    let i = (v * image.width + u) * 4;
                    let p = &image.pixels[i..i + 4];
                    self.blend(px, py, [p[0], p[1], p[2], p[3]], 1.0);
                }
            }
        }
    }

    pub fn png(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let width = u32::try_from(self.width).ok()?;
        let height = u32::try_from(self.height).ok()?;
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&self.pixels).ok()?;
        writer.finish().ok()?;
        Some(out)
    }
}

/// a decoded 8-bit RGBA image, e.g. the logo
pub struct RgbaImage {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl RgbaImage {
    /// decode an 8-bit RGBA PNG
    pub fn decode_png(bytes: &[u8]) -> Option<Self> {
        let mut reader = png::Decoder::new(bytes).read_info().ok()?;
        let mut pixels = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).ok()?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return None;
        }
        pixels.truncate(info.buffer_size());
        Some(Self {
            width: usize::try_from(info.width).ok()?,
            height: usize::try_from(info.height).ok()?,
            pixels,
        })
    }
}
