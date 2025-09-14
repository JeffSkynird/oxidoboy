pub const DEFAULT_W: usize = 160;
pub const DEFAULT_H: usize = 144;

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum Key {
    Up = 0,
    Down,
    Left,
    Right,
    A,
    B,
    Start,
    Select,
}

pub fn key_bit(k: Key) -> u32 {
    1u32 << (k as u32)
}

// Color helpers RGBA packed (little-endian in bytes) 
#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((a as u32) << 24) | ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
}

// GB-like palette
pub const P0: u32 = rgba(15, 56, 15, 255);
pub const P1: u32 = rgba(48, 98, 48, 255);
pub const P2: u32 = rgba(139, 172, 15, 255);
pub const P3: u32 = rgba(155, 188, 15, 255);

// Drawing utilities (game side, over WASM framebuffer)
pub struct Frame<'a> {
    pub data: &'a mut [u8],
    pub w: usize,
    pub h: usize,
}
impl<'a> Frame<'a> {
    pub fn clear(&mut self, color: u32) {
        let bytes = color.to_le_bytes();
        for px in self.data.chunks_exact_mut(4) {
            px.copy_from_slice(&bytes);
        }
    }
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (W, H) = (self.w as i32, self.h as i32);
        let bytes = color.to_le_bytes();
        for yy in y.max(0)..(y + h).min(H) {
            for xx in x.max(0)..(x + w).min(W) {
                let idx = ((yy as usize) * self.w + (xx as usize)) * 4;
                self.data[idx..idx + 4].copy_from_slice(&bytes);
            }
        }
    }
}

// --- Palettes and Sprites -----------------------------------------------

#[derive(Clone, Copy)]
pub struct Palette(pub [u32; 4]);

impl Palette {
    pub const GB: Palette = Palette([P0, P1, P2, P3]);
    #[inline] pub fn color(&self, i: u8) -> u32 { self.0[i as usize] }
}

pub struct SpriteAtlas {
    pub w: usize,        // total width of the atlas in pixels
    pub h: usize,        // total height of the atlas in pixels
    pub tile_w: usize,   // width of each tile
    pub tile_h: usize,   // height of each tile
    pub pixels: Vec<u8>, // indexes 0..=3 per pixel
}

impl SpriteAtlas {
    /// Creates an atlas from an indexed (0..=3) buffer of size w*h.
    pub fn from_indexed(pixels: Vec<u8>, w: usize, h: usize, tile_w: usize, tile_h: usize) -> Self {
        assert_eq!(pixels.len(), w * h, "pixels must be w*h");
        assert!(tile_w > 0 && tile_h > 0 && w % tile_w == 0 && h % tile_h == 0, "tiles must divide atlas");
        Self { w, h, tile_w, tile_h, pixels }
    }

    /// Draws tile `tile_id` at (dx,dy). `index 0` is treated as transparent if `transparent_zero` is true.
    pub fn blit(&self, frame: &mut Frame, dx: i32, dy: i32, tile_id: usize, pal: &Palette,
                flip_x: bool, flip_y: bool, transparent_zero: bool) {
        let tiles_x = self.w / self.tile_w;
        let sx = (tile_id % tiles_x) * self.tile_w;
        let sy = (tile_id / tiles_x) * self.tile_h;

        for ty in 0..self.tile_h {
            for tx in 0..self.tile_w {
                let sxp = if flip_x { (self.tile_w - 1) - tx } else { tx };
                let syp = if flip_y { (self.tile_h - 1) - ty } else { ty };
                let src_x = sx + sxp;
                let src_y = sy + syp;

                let idx = self.pixels[src_y * self.w + src_x];
                if transparent_zero && idx == 0 { continue; }
                let color = pal.color((idx & 0b11) as u8);

                let x = dx + tx as i32;
                let y = dy + ty as i32;
                if x < 0 || y < 0 || x >= frame.w as i32 || y >= frame.h as i32 { continue; }
                let di = ((y as usize) * frame.w + (x as usize)) * 4;
                frame.data[di..di+4].copy_from_slice(&color.to_le_bytes());
            }
        }
    }
}

// --- TileMap (background with tilemap and scrolling) -------------------
pub struct TileMap {
    pub w: usize,        // width in tiles
    pub h: usize,        // high in tiles
    pub tile_w: usize,   // tile width in px
    pub tile_h: usize,   // tile height in px
    pub tiles: Vec<usize>, // tile ids (index the atlas)
}

impl TileMap {
    pub fn new(w: usize, h: usize, tile_w: usize, tile_h: usize, tiles: Vec<usize>) -> Self {
        assert_eq!(tiles.len(), w * h, "len(tiles) must be w*h");
        Self { w, h, tile_w, tile_h, tiles }
    }

    /// Draw the map with pixel scroll (scroll_x, scroll_y).
    /// If `transparent_zero` is true, atlas index 0 is treated as transparent.
    pub fn draw(
        &self,
        frame: &mut Frame,
        atlas: &SpriteAtlas,
        pal: &Palette,
        scroll_x: i32,
        scroll_y: i32,
        transparent_zero: bool,
    ) {
        let tw = self.tile_w as i32;
        let th = self.tile_h as i32;
        let vw = frame.w as i32;
        let vh = frame.h as i32;

        // Offset in pixels within the first visible tile
        let off_x = ((scroll_x % tw) + tw) % tw;
        let off_y = ((scroll_y % th) + th) % th;
        // Base tile in the map (with wrap)
        let base_c = (scroll_x.div_euclid(tw)).rem_euclid(self.w as i32);
        let base_r = (scroll_y.div_euclid(th)).rem_euclid(self.h as i32);

        // +2 to cover edges when there's partial offset
        let cols = vw / tw + 2;
        let rows = vh / th + 2;

        for r in 0..rows {
            let y = r * th - off_y;
            let map_r = (base_r + r).rem_euclid(self.h as i32) as usize;
            for c in 0..cols {
                let x = c * tw - off_x;
                let map_c = (base_c + c).rem_euclid(self.w as i32) as usize;
                let tile_id = self.tiles[map_r * self.w + map_c];
                atlas.blit(frame, x, y, tile_id, pal, false, false, transparent_zero);
            }
        }
    }
}

// ====================== Texto 5x7 (HUD) ======================
impl<'a> Frame<'a> {
    /// Draw monospaced 5x7 text. Supports: A-Z, 0-9, space, .:-!/?
    /// `color`: RGBA (usa P1..P3 o pal.color(i)).
    pub fn text5x7(&mut self, x: i32, y: i32, text: &str, color: u32) {
        let mut cx = x;
        for ch in text.chars() {
            self.char5x7(cx, y, ch, color);
            cx += 6; // 5 px width + 1 px spacing
        }
    }

    fn char5x7(&mut self, x: i32, y: i32, ch: char, color: u32) {
        if let Some(rows) = glyph5x7(ch) {
            for (dy, row) in rows.iter().enumerate() {
                // 5 bits useful, from MSB to LSB (bit 4 → x, bit 0 → x+4)
                for dx in 0..5 {
                    if ((row >> (4 - dx)) & 1) != 0 {
                        // an individual pixel: use rect 1x1 to avoid touching internals
                        self.rect(x + dx as i32, y + dy as i32, 1, 1, color);
                    }
                }
            }
        }
    }
}

/// Return 7 rows (bits) for the character, or None if not supported.
/// Font 5x7 basic (subset: digits, uppercase and some symbols).
fn glyph5x7(ch: char) -> Option<[u8; 7]> {
    let c = ch.to_ascii_uppercase();
    let g = match c {
        // Space and punctuation
        ' ' => [0,0,0,0,0,0,0],
        '.' => [0,0,0,0,0,0b00100,0],
        ':' => [0,0,0b00100,0,0b00100,0,0],
        '-' => [0,0,0,0b11111,0,0,0],
        '/' => [0,0b00001,0b00010,0b00100,0b01000,0b10000,0],
        '!' => [0b00100,0b00100,0b00100,0b00100,0,0b00100,0],
        '?' => [0b01110,0b10001,0b00010,0b00100,0b00100,0,0b00100],

        // Numbers 0-9
        '0' => [0b01110,0b10001,0b10011,0b10101,0b11001,0b10001,0b01110],
        '1' => [0b00100,0b01100,0b00100,0b00100,0b00100,0b00100,0b01110],
        '2' => [0b01110,0b10001,0b00001,0b00110,0b01000,0b10000,0b11111],
        '3' => [0b11110,0b00001,0b00001,0b01110,0b00001,0b00001,0b11110],
        '4' => [0b00010,0b00110,0b01010,0b10010,0b11111,0b00010,0b00010],
        '5' => [0b11111,0b10000,0b11110,0b00001,0b00001,0b10001,0b01110],
        '6' => [0b00110,0b01000,0b10000,0b11110,0b10001,0b10001,0b01110],
        '7' => [0b11111,0b00001,0b00010,0b00100,0b01000,0b01000,0b01000],
        '8' => [0b01110,0b10001,0b10001,0b01110,0b10001,0b10001,0b01110],
        '9' => [0b01110,0b10001,0b10001,0b01111,0b00001,0b00010,0b01100],

        // Letters A-Z (5x7)
        'A' => [0b01110,0b10001,0b10001,0b11111,0b10001,0b10001,0b10001],
        'B' => [0b11110,0b10001,0b10001,0b11110,0b10001,0b10001,0b11110],
        'C' => [0b01110,0b10001,0b10000,0b10000,0b10000,0b10001,0b01110],
        'D' => [0b11100,0b10010,0b10001,0b10001,0b10001,0b10010,0b11100],
        'E' => [0b11111,0b10000,0b10000,0b11110,0b10000,0b10000,0b11111],
        'F' => [0b11111,0b10000,0b10000,0b11110,0b10000,0b10000,0b10000],
        'G' => [0b01110,0b10001,0b10000,0b10111,0b10001,0b10001,0b01110],
        'H' => [0b10001,0b10001,0b10001,0b11111,0b10001,0b10001,0b10001],
        'I' => [0b01110,0b00100,0b00100,0b00100,0b00100,0b00100,0b01110],
        'J' => [0b00001,0b00001,0b00001,0b00001,0b10001,0b10001,0b01110],
        'K' => [0b10001,0b10010,0b10100,0b11000,0b10100,0b10010,0b10001],
        'L' => [0b10000,0b10000,0b10000,0b10000,0b10000,0b10000,0b11111],
        'M' => [0b10001,0b11011,0b10101,0b10101,0b10001,0b10001,0b10001],
        'N' => [0b10001,0b11001,0b10101,0b10011,0b10001,0b10001,0b10001],
        'O' => [0b01110,0b10001,0b10001,0b10001,0b10001,0b10001,0b01110],
        'P' => [0b11110,0b10001,0b10001,0b11110,0b10000,0b10000,0b10000],
        'Q' => [0b01110,0b10001,0b10001,0b10001,0b10101,0b10010,0b01101],
        'R' => [0b11110,0b10001,0b10001,0b11110,0b10100,0b10010,0b10001],
        'S' => [0b01111,0b10000,0b10000,0b01110,0b00001,0b00001,0b11110],
        'T' => [0b11111,0b00100,0b00100,0b00100,0b00100,0b00100,0b00100],
        'U' => [0b10001,0b10001,0b10001,0b10001,0b10001,0b10001,0b01110],
        'V' => [0b10001,0b10001,0b10001,0b10001,0b10001,0b01010,0b00100],
        'W' => [0b10001,0b10001,0b10001,0b10101,0b10101,0b11011,0b10001],
        'X' => [0b10001,0b10001,0b01010,0b00100,0b01010,0b10001,0b10001],
        'Y' => [0b10001,0b01010,0b00100,0b00100,0b00100,0b00100,0b00100],
        'Z' => [0b11111,0b00001,0b00010,0b00100,0b01000,0b10000,0b11111],
        _ => return None,
    };
    Some(g)
}
