//! The console font.
//!
//! Two faces are supported, and the client picks whichever it can get:
//!
//! * [`Face::from_charset`] wraps `gfx/2d/bigchars.tga` from `baseq3` — the
//!   real thing, a 256x256 sheet of 16x16 texel cells drawn 8x16 pixels, which
//!   is what `SCR_DrawSmallChar` stretches onto the screen;
//! * [`Face::builtin`] is a 5x7 bitmap face compiled into the binary, so the
//!   console still has glyphs when there is no game data to mount.
//!
//! Either way a glyph is a textured quad through the 2D pipeline, batched with
//! everything else in the frame.

use crate::vk::Vertex2D;

/// Width of one built-in glyph, in font pixels.
pub const GLYPH_WIDTH: u32 = 5;
/// Height of one built-in glyph, in font pixels.
pub const GLYPH_HEIGHT: u32 = 7;
/// Built-in atlas cell size. The extra pixels are padding, so neighbouring
/// glyphs cannot bleed into each other when the sampler filters.
pub const CELL: u32 = 8;
/// Cells across the built-in atlas.
pub const COLUMNS: u32 = 16;
/// Cells down the built-in atlas.
pub const ROWS: u32 = 6;

/// Cells across a game charset: `ch & 15` selects the column, as
/// `SCR_DrawSmallChar` does.
pub const CHARSET_COLUMNS: u32 = 16;
/// Cells down a game charset: `ch >> 4` selects the row.
pub const CHARSET_ROWS: u32 = 16;

/// First character in the built-in table.
pub const FIRST_CHAR: u8 = b' ';
/// Last character in the built-in table.
pub const LAST_CHAR: u8 = b'~';

/// Glyph bitmaps, one per printable ASCII character from space to tilde.
/// Each entry is seven rows of five cells, `#` for ink and `.` for paper.
const GLYPHS: [&str; 95] = [
    "...../...../...../...../...../...../.....",
    "..#../..#../..#../..#../..#../...../..#..",
    ".#.#./.#.#./...../...../...../...../.....",
    ".#.#./#####/.#.#./.#.#./#####/.#.#./.....",
    "..#../.####/#..../.###./....#/####./..#..",
    "##..#/##..#/...#./..#../.#.../#..##/#..##",
    ".##../#..#./#.#../.#.../#.#.#/#..#./.##.#",
    "..#../..#../...../...../...../...../.....",
    "...#./..#../.#.../.#.../.#.../..#../...#.",
    ".#.../..#../...#./...#./...#./..#../.#...",
    "...../#.#.#/.###./#####/.###./#.#.#/.....",
    "...../..#../..#../#####/..#../..#../.....",
    "...../...../...../...../...../..#../.#...",
    "...../...../...../#####/...../...../.....",
    "...../...../...../...../...../.##../.##..",
    "....#/...#./...#./..#../.#.../.#.../#....",
    ".###./#...#/#..##/#.#.#/##..#/#...#/.###.",
    "..#../.##../..#../..#../..#../..#../.###.",
    ".###./#...#/....#/...#./..#../.#.../#####",
    "#####/...#./..#../...#./....#/#...#/.###.",
    "...#./..##./.#.#./#..#./#####/...#./...#.",
    "#####/#..../####./....#/....#/#...#/.###.",
    "..##./.#.../#..../####./#...#/#...#/.###.",
    "#####/....#/...#./..#../.#.../.#.../.#...",
    ".###./#...#/#...#/.###./#...#/#...#/.###.",
    ".###./#...#/#...#/.####/....#/...#./.##..",
    "...../.##../.##../...../.##../.##../.....",
    "...../.##../.##../...../.##../.##../.#...",
    "...#./..#../.#.../#..../.#.../..#../...#.",
    "...../...../#####/...../#####/...../.....",
    ".#.../..#../...#./....#/...#./..#../.#...",
    ".###./#...#/....#/...#./..#../...../..#..",
    ".###./#...#/....#/.##.#/#.#.#/#.#.#/.###.",
    ".###./#...#/#...#/#####/#...#/#...#/#...#",
    "####./#...#/#...#/####./#...#/#...#/####.",
    ".###./#...#/#..../#..../#..../#...#/.###.",
    "###../#..#./#...#/#...#/#...#/#..#./###..",
    "#####/#..../#..../####./#..../#..../#####",
    "#####/#..../#..../####./#..../#..../#....",
    ".###./#...#/#..../#.###/#...#/#...#/.####",
    "#...#/#...#/#...#/#####/#...#/#...#/#...#",
    ".###./..#../..#../..#../..#../..#../.###.",
    "..###/...#./...#./...#./...#./#..#./.##..",
    "#...#/#..#./#.#../##.../#.#../#..#./#...#",
    "#..../#..../#..../#..../#..../#..../#####",
    "#...#/##.##/#.#.#/#.#.#/#...#/#...#/#...#",
    "#...#/##..#/#.#.#/#..##/#...#/#...#/#...#",
    ".###./#...#/#...#/#...#/#...#/#...#/.###.",
    "####./#...#/#...#/####./#..../#..../#....",
    ".###./#...#/#...#/#...#/#.#.#/#..#./.##.#",
    "####./#...#/#...#/####./#.#../#..#./#...#",
    ".####/#..../#..../.###./....#/....#/####.",
    "#####/..#../..#../..#../..#../..#../..#..",
    "#...#/#...#/#...#/#...#/#...#/#...#/.###.",
    "#...#/#...#/#...#/#...#/#...#/.#.#./..#..",
    "#...#/#...#/#...#/#.#.#/#.#.#/##.##/#...#",
    "#...#/#...#/.#.#./..#../.#.#./#...#/#...#",
    "#...#/#...#/.#.#./..#../..#../..#../..#..",
    "#####/....#/...#./..#../.#.../#..../#####",
    ".##../.#.../.#.../.#.../.#.../.#.../.##..",
    "#..../.#.../.#.../..#../...#./...#./....#",
    ".##../...#./...#./...#./...#./...#./.##..",
    "..#../.#.#./#...#/...../...../...../.....",
    "...../...../...../...../...../...../#####",
    ".#.../..#../...../...../...../...../.....",
    "...../...../.###./....#/.####/#...#/.####",
    "#..../#..../####./#...#/#...#/#...#/####.",
    "...../...../.###./#..../#..../#...#/.###.",
    "....#/....#/.####/#...#/#...#/#...#/.####",
    "...../...../.###./#...#/#####/#..../.###.",
    "..##./.#..#/.#.../####./.#.../.#.../.#...",
    "...../.####/#...#/#...#/.####/....#/.###.",
    "#..../#..../####./#...#/#...#/#...#/#...#",
    "..#../...../.##../..#../..#../..#../.###.",
    "...#./...../..##./...#./...#./#..#./.##..",
    "#..../#..../#..#./#.#../##.../##.../#.#..",
    ".##../..#../..#../..#../..#../..#../.###.",
    "...../...../##.#./#.#.#/#.#.#/#...#/#...#",
    "...../...../####./#...#/#...#/#...#/#...#",
    "...../...../.###./#...#/#...#/#...#/.###.",
    "...../####./#...#/#...#/####./#..../#....",
    "...../.####/#...#/#...#/.####/....#/....#",
    "...../...../#.##./##..#/#..../#..../#....",
    "...../...../.####/#..../.###./....#/####.",
    ".#.../.#.../####./.#.../.#.../.#..#/..##.",
    "...../...../#...#/#...#/#...#/#..##/.##.#",
    "...../...../#...#/#...#/#...#/.#.#./..#..",
    "...../...../#...#/#...#/#.#.#/#.#.#/.#.#.",
    "...../...../#...#/.#.#./..#../.#.#./#...#",
    "...../...../#...#/#...#/.####/....#/.###.",
    "...../...../#####/...#./..#../.#.../#####",
    "...##/..#../..#../.#.../..#../..#../...##",
    "..#../..#../..#../..#../..#../..#../..#..",
    "##.../..#../..#../...#./..#../..#../##...",
    "...../...../.#..#/#.#.#/#..#./...../.....",
];

/// One cell of a font atlas: its column and row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub column: u32,
    pub row: u32,
}

/// How a face lays its glyphs out in the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Packed from the first printable character, used by the built-in face:
    /// the glyph for `'!'` is the second cell, whatever its byte value is.
    Packed,
    /// The engine's charset order, `ch & 15` across and `ch >> 4` down, which
    /// is what `SCR_DrawSmallChar` indexes.
    Grid,
}

/// A bitmap face: an atlas plus the metrics needed to draw text with it.
#[derive(Debug)]
pub struct Face {
    layout: Layout,
    /// RGBA8 atlas, top row first.
    pub atlas: Vec<u8>,
    pub atlas_width: u32,
    pub atlas_height: u32,
    columns: u32,
    rows: u32,
    /// Size of one cell in atlas texels.
    cell_texels: (u32, u32),
    /// Size of the inked part of a cell, in atlas texels. A game charset inks
    /// the whole cell; the built-in face leaves padding around its glyphs.
    glyph_texels: (u32, u32),
    /// Screen size of a glyph at scale 1.
    glyph_size: (f32, f32),
    /// Distance between two glyph origins at scale 1.
    advance: f32,
    /// Distance between two text lines at scale 1.
    line_height: f32,
}

impl Face {
    /// The compiled-in 5x7 face, for a client with no game data.
    pub fn builtin() -> Self {
        let (atlas_width, atlas_height, atlas) = builtin_atlas();
        Face {
            layout: Layout::Packed,
            atlas,
            atlas_width,
            atlas_height,
            columns: COLUMNS,
            rows: ROWS,
            cell_texels: (CELL, CELL),
            glyph_texels: (GLYPH_WIDTH, GLYPH_HEIGHT),
            glyph_size: (GLYPH_WIDTH as f32, GLYPH_HEIGHT as f32),
            advance: (GLYPH_WIDTH + 1) as f32,
            line_height: (GLYPH_HEIGHT + 3) as f32,
        }
    }

    /// Wraps a decoded charset, the way `cls.charSetShader` is used: a grid of
    /// 16x16 cells, each drawn 8 pixels wide and 16 tall.
    ///
    /// The atlas must divide evenly into the 16x16 grid; anything else is not
    /// a charset and is refused rather than drawn skewed.
    pub fn from_charset(width: u32, height: u32, atlas: Vec<u8>) -> Result<Self, String> {
        if width % CHARSET_COLUMNS != 0 || height % CHARSET_ROWS != 0 {
            return Err(format!(
                "charset {width}x{height} does not divide into {CHARSET_COLUMNS}x{CHARSET_ROWS} cells"
            ));
        }
        let cell = (width / CHARSET_COLUMNS, height / CHARSET_ROWS);
        if atlas.len() != (width * height * 4) as usize {
            return Err(format!(
                "charset {width}x{height} has {} bytes, expected {}",
                atlas.len(),
                width * height * 4
            ));
        }
        Ok(Face {
            layout: Layout::Grid,
            atlas,
            atlas_width: width,
            atlas_height: height,
            columns: CHARSET_COLUMNS,
            rows: CHARSET_ROWS,
            cell_texels: cell,
            glyph_texels: cell,
            // SCR_DrawSmallChar's on-screen size: SMALLCHAR_WIDTH x
            // SMALLCHAR_HEIGHT, from a cell twice as wide in texels.
            glyph_size: (cell.0 as f32 / 2.0, cell.1 as f32),
            advance: cell.0 as f32 / 2.0,
            line_height: cell.1 as f32,
        })
    }

    /// Atlas cell for a character. Control characters and anything outside the
    /// font fall back to a space, so a stray byte cannot panic.
    pub fn cell(&self, ch: char) -> Cell {
        let byte = ch as u32;
        let index = match self.layout {
            Layout::Packed => {
                if !(FIRST_CHAR as u32..=LAST_CHAR as u32).contains(&byte) {
                    return Cell { column: 0, row: 0 };
                }
                byte - FIRST_CHAR as u32
            }
            Layout::Grid => {
                if byte > 255 {
                    return Cell { column: 0, row: 0 };
                }
                byte
            }
        };
        let cell = Cell {
            column: index % self.columns,
            row: index / self.columns,
        };
        if cell.row >= self.rows {
            return Cell { column: 0, row: 0 };
        }
        cell
    }

    /// Top-left corner of a cell in atlas texels.
    fn cell_origin(&self, cell: Cell) -> (u32, u32) {
        (cell.column * self.cell_texels.0, cell.row * self.cell_texels.1)
    }

    pub fn advance(&self, scale: f32) -> f32 {
        self.advance * scale
    }

    pub fn line_height(&self, scale: f32) -> f32 {
        self.line_height * scale
    }

    /// Width of a string in screen pixels at a scale.
    pub fn width(&self, text: &str, scale: f32) -> f32 {
        text.chars().count() as f32 * self.advance(scale)
    }

    /// Scale text should be drawn at for a window height. The built-in face is
    /// doubled on VGA and up; a game charset is already 8x16 and is drawn as
    /// the engine draws it.
    pub fn default_scale(&self, window_height: u32) -> f32 {
        match self.layout {
            Layout::Packed if window_height >= 480 => 2.0,
            _ => 1.0,
        }
    }

    /// Appends one textured quad per character to `quads`.
    ///
    /// `(x, y)` is the top-left corner of the first glyph. Coordinates are
    /// screen pixels with the origin at the top left, the same space
    /// `DrawStretchPic` uses, so text lands where the caller measured it.
    pub fn push_text(
        &self,
        quads: &mut Vec<Vertex2D>,
        text: &str,
        x: f32,
        y: f32,
        scale: f32,
        color: [u8; 4],
    ) {
        let (atlas_width, atlas_height) = (self.atlas_width as f32, self.atlas_height as f32);
        let (glyph_w, glyph_h) = (
            self.glyph_texels.0 as f32,
            self.glyph_texels.1 as f32,
        );

        let mut pen = x;
        for ch in text.chars() {
            if ch == '\t' {
                // Four spaces' worth of movement, as the console assumes.
                pen += self.advance(scale) * 4.0;
                continue;
            }
            if ch == ' ' {
                pen += self.advance(scale);
                continue;
            }

            let (origin_x, origin_y) = self.cell_origin(self.cell(ch));
            let u0 = origin_x as f32 / atlas_width;
            let v0 = origin_y as f32 / atlas_height;
            let u1 = (origin_x as f32 + glyph_w) / atlas_width;
            let v1 = (origin_y as f32 + glyph_h) / atlas_height;

            let (x0, y0) = (pen, y);
            let (x1, y1) = (
                pen + self.glyph_size.0 * scale,
                y + self.glyph_size.1 * scale,
            );
            let vertex = |px: f32, py: f32, u: f32, v: f32| Vertex2D {
                pos: [px, py],
                uv: [u, v],
                color,
            };
            quads.extend_from_slice(&[
                vertex(x0, y0, u0, v0),
                vertex(x1, y0, u1, v0),
                vertex(x1, y1, u1, v1),
                vertex(x0, y0, u0, v0),
                vertex(x1, y1, u1, v1),
                vertex(x0, y1, u0, v1),
            ]);

            pen += self.advance(scale);
        }
    }
}

/// Renders the built-in glyph table into an RGBA8 atlas: white ink,
/// transparent paper.
fn builtin_atlas() -> (u32, u32, Vec<u8>) {
    let (width, height) = (COLUMNS * CELL, ROWS * CELL);
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    for (index, glyph) in GLYPHS.iter().enumerate() {
        let column = index as u32 % COLUMNS;
        let row = index as u32 / COLUMNS;
        for (y, line) in glyph.split('/').enumerate() {
            for (x, pixel) in line.chars().enumerate() {
                if pixel != '#' {
                    continue;
                }
                let px = column * CELL + x as u32;
                let py = row * CELL + y as u32;
                let offset = ((py * width + px) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }

    (width, height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_is_seven_rows_of_five() {
        assert_eq!(GLYPHS.len(), (LAST_CHAR - FIRST_CHAR) as usize + 1);
        for (index, glyph) in GLYPHS.iter().enumerate() {
            let rows: Vec<&str> = glyph.split('/').collect();
            assert_eq!(rows.len(), 7, "glyph {} has {} rows", index, rows.len());
            for row in rows {
                assert_eq!(row.chars().count(), 5, "glyph {} has a short row", index);
                assert!(row.chars().all(|c| c == '#' || c == '.'), "glyph {index}");
            }
        }
    }

    #[test]
    fn builtin_cells_follow_reading_order() {
        let face = Face::builtin();
        assert_eq!(face.cell(' '), Cell { column: 0, row: 0 });
        assert_eq!(face.cell('!'), Cell { column: 1, row: 0 });
        assert_eq!(face.cell('~'), Cell { column: 14, row: 5 });
        // Anything unprintable draws the blank first cell.
        assert_eq!(face.cell('\n'), Cell { column: 0, row: 0 });
        assert_eq!(face.cell('é'), Cell { column: 0, row: 0 });
    }

    #[test]
    fn builtin_atlas_has_ink_where_the_glyph_does() {
        let face = Face::builtin();
        assert_eq!((face.atlas_width, face.atlas_height), (128, 48));
        assert_eq!(face.atlas.len(), (128 * 48 * 4) as usize);

        let ink = |px: u32, py: u32| face.atlas[((py * face.atlas_width + px) * 4 + 3) as usize] == 255;
        let (x, y) = face.cell_origin(face.cell('A'));
        assert!(ink(x + 1, y), "'A' should start with ink on its first row");
        assert!(!ink(x + GLYPH_WIDTH, y), "padding must stay transparent");
    }

    #[test]
    fn text_is_one_quad_per_visible_character() {
        let face = Face::builtin();
        let mut quads = Vec::new();
        face.push_text(&mut quads, "ok", 10.0, 20.0, 2.0, [255; 4]);
        assert_eq!(quads.len(), 12);
        assert_eq!(quads[0].pos, [10.0, 20.0]);
        assert_eq!(quads[6].pos[0], 10.0 + face.advance(2.0));
        assert_eq!(face.width("ok", 2.0), face.advance(2.0) * 2.0);
    }

    #[test]
    fn spaces_advance_without_drawing() {
        let face = Face::builtin();
        let mut quads = Vec::new();
        face.push_text(&mut quads, "a b", 0.0, 0.0, 1.0, [255; 4]);
        assert_eq!(quads.len(), 12);
        assert_eq!(quads[6].pos[0], face.advance(1.0) * 2.0);
    }

    /// Builds a 16x16-cell charset with one lit texel in the cell for `ch`.
    fn charset_with_dot(ch: char) -> Face {
        let (width, height) = (256u32, 256u32);
        let mut atlas = vec![0u8; (width * height * 4) as usize];
        let cell = Face {
            layout: Layout::Grid,
            atlas: Vec::new(),
            atlas_width: width,
            atlas_height: height,
            columns: CHARSET_COLUMNS,
            rows: CHARSET_ROWS,
            cell_texels: (16, 16),
            glyph_texels: (16, 16),
            glyph_size: (8.0, 16.0),
            advance: 8.0,
            line_height: 16.0,
        };
        let unique = cell.cell(ch);
        let offset = (((unique.row * 16 + 1) * width + unique.column * 16 + 1) * 4) as usize;
        atlas[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
        Face::from_charset(width, height, atlas).unwrap()
    }

    #[test]
    fn a_charset_maps_characters_across_sixteen_cells() {
        let face = Face::builtin();
        // The game layout is `ch & 15`, `ch >> 4`, not a running index.
        let charset = Face::from_charset(256, 256, vec![0u8; 256 * 256 * 4]).unwrap();
        assert_eq!(charset.cell('A'), Cell { column: 1, row: 4 });
        assert_eq!(charset.cell(' '), Cell { column: 0, row: 2 });
        assert_eq!(charset.cell('~'), Cell { column: 14, row: 7 });
        assert_ne!(face.cell('A'), charset.cell('A'));
    }

    #[test]
    fn a_charset_draws_eight_by_sixteen_glyphs() {
        let face = charset_with_dot('A');
        assert_eq!(face.advance(1.0), 8.0);
        assert_eq!(face.line_height(1.0), 16.0);
        assert_eq!(face.width("AB", 1.0), 16.0);
        // One scale: the engine's console size, whatever the window height.
        assert_eq!(face.default_scale(1080), 1.0);

        let mut quads = Vec::new();
        face.push_text(&mut quads, "A", 0.0, 0.0, 1.0, [255; 4]);
        assert_eq!(quads.len(), 6);
        let cell = face.cell('A');
        let (origin_x, origin_y) = face.cell_origin(cell);
        assert!((quads[0].uv[0] - origin_x as f32 / 256.0).abs() < 1e-6);
        assert!((quads[0].uv[1] - origin_y as f32 / 256.0).abs() < 1e-6);
        assert_eq!(quads[2].pos, [8.0, 16.0]);
    }

    #[test]
    fn a_bad_charset_is_refused() {
        assert!(Face::from_charset(100, 256, vec![0u8; 100 * 256 * 4]).is_err());
        assert!(Face::from_charset(256, 256, vec![0u8; 16]).is_err());
    }
}
