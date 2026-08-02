//! WebGL2 terminal grid renderer — the hot path of the web client.
//!
//! # Design
//!
//! A terminal frame is a few thousand coloured rectangles plus a few thousand
//! textured quads. Anything that touches the DOM or the GL driver per *cell*
//! loses, so this module reduces a whole [`GridSnapshot`] to exactly **two**
//! `drawArrays` calls:
//!
//! 1. **backgrounds** — every non-default cell background, the selection wash
//!    and the block cursor fill, as solid quads sampling a reserved opaque
//!    white texel in the atlas;
//! 2. **glyphs** — every visible glyph, plus underlines and the bar/outline
//!    cursor (again via the white texel).
//!
//! Both passes share one shader, one texture and one streamed vertex buffer;
//! the only per-pass GL state change is the buffer upload. Vertex scratch
//! `Vec<f32>`s live on the struct and are cleared (not freed) each frame, so
//! steady-state rendering allocates nothing.
//!
//! Glyphs are rasterised on demand onto a detached 2d canvas (never attached to
//! the document — attaching would force layout) and blitted into a shelf-packed
//! RGBA atlas. The atlas stores coverage in the alpha channel only; colour
//! comes from the vertex stream, so one raster serves every colour a glyph is
//! ever drawn in. Cache key is `(char, bold, italic)` — dim/inverse/underline
//! are colour-level effects and need no separate raster.
//!
//! Everything is rasterised and laid out in **device** pixels (css × dpr) and
//! cell dimensions are integers there, so cell edges land on physical pixels at
//! any DPR. CSS-space metrics are derived by division, never the other way.
//!
//! All web API failures are surfaced as `JsValue` errors; a lost GL context is
//! detected and turns `render` into a no-op rather than a panic.

use std::collections::HashMap;

use js_sys::Float32Array;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlBuffer,
    WebGlProgram, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
};

use seance_core::snapshot::GridSnapshot;

/// Sentinel meaning "use the theme default" for `CellSnap::fg` / `bg`.
const DEFAULT_COLOR: u32 = 0xFFFF_FFFF;

// Candlelit palette (docs/THEME.md). Kept as linear-free sRGB f32 triples:
// the GL surface is not sRGB-encoded, so these go straight to the wire.
const BG: [f32; 3] = rgb(0x13, 0x11, 0x11); // #131111
const FG: [f32; 3] = rgb(0xEB, 0xE3, 0xDB); // #EBE3DB
const FLAME: [f32; 3] = rgb(0xE9, 0xA0, 0x3A); // #E9A03A
const GHOST: [f32; 3] = rgb(0x69, 0x60, 0x5D); // #69605D

/// ANSI-dim is a straight multiply on the resolved foreground.
const DIM_FACTOR: f32 = 0.62;

const ATLAS_START: i32 = 1024;
const ATLAS_MAX: i32 = 4096;

/// Floats per vertex: pos(2) + rgba(4) + uv(2).
const FLOATS_PER_VERT: usize = 8;
const FLOATS_PER_QUAD: usize = FLOATS_PER_VERT * 6;

const fn rgb(r: u8, g: u8, b: u8) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

fn unpack(c: u32, default: [f32; 3]) -> [f32; 3] {
    if c == DEFAULT_COLOR {
        return default;
    }
    rgb(
        ((c >> 16) & 0xFF) as u8,
        ((c >> 8) & 0xFF) as u8,
        (c & 0xFF) as u8,
    )
}

/// Per-frame options the integrator owns (blink phase, focus, mouse selection).
pub struct RenderOpts {
    pub focused: bool,
    pub cursor_visible: bool,
    /// Inclusive linear cell-index range drawn with fg/bg swapped.
    pub selection: Option<(usize, usize)>,
}

/// One rasterised glyph in the atlas.
#[derive(Clone, Copy)]
struct Glyph {
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    /// Bitmap size in device px. Height is always the full glyph box.
    w: f32,
    h: f32,
}

/// Font-derived geometry, all device pixels except the `_css` pair.
#[derive(Clone, Copy)]
struct Metrics {
    cell_w: i32,
    cell_h: i32,
    ascent: i32,
    /// Bleed around each glyph box, absorbing overhang and filtering slop.
    pad: i32,
    /// Widest glyph bitmap we will rasterise (wide/CJK glyphs need the room).
    box_w: i32,
    cell_w_css: f32,
    cell_h_css: f32,
}

pub struct TermRenderer {
    canvas: HtmlCanvasElement,
    gl: Gl,
    program: WebGlProgram,
    u_res: Option<WebGlUniformLocation>,
    vao: Option<WebGlVertexArrayObject>,
    vbo: WebGlBuffer,

    // Glyph rasterisation
    raster: HtmlCanvasElement,
    ctx2d: CanvasRenderingContext2d,
    family: String,
    font_px: f32,
    dpr: f64,
    metrics: Metrics,

    // Atlas
    tex: WebGlTexture,
    atlas_size: i32,
    shelf_x: i32,
    shelf_y: i32,
    shelf_h: i32,
    glyphs: HashMap<(char, bool, bool), Glyph>,
    /// UV of an opaque texel used by every solid (untextured) quad.
    white_uv: (f32, f32),
    /// Set when a glyph allocation forced an atlas grow mid-frame; the frame is
    /// rebuilt once because previously emitted UVs are now stale.
    atlas_dirty: bool,

    // Surface
    dev_w: i32,
    dev_h: i32,

    // Reused vertex scratch
    bg_verts: Vec<f32>,
    fg_verts: Vec<f32>,
    staging: Float32Array,
}

const VERT_SRC: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec4 a_color;
layout(location = 2) in vec2 a_uv;
uniform vec2 u_res;
out vec4 v_color;
out vec2 v_uv;
void main() {
    vec2 ndc = (a_pos / u_res) * 2.0 - 1.0;
    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);
    v_color = a_color;
    v_uv = a_uv;
}
"#;

// The atlas carries coverage in alpha only, so tint is pure vertex colour.
const FRAG_SRC: &str = r#"#version 300 es
precision highp float;
in vec4 v_color;
in vec2 v_uv;
uniform sampler2D u_tex;
out vec4 o_color;
void main() {
    o_color = vec4(v_color.rgb, v_color.a * texture(u_tex, v_uv).a);
}
"#;

impl TermRenderer {
    pub fn new(canvas: HtmlCanvasElement) -> Result<TermRenderer, JsValue> {
        let win = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
        let doc = win
            .document()
            .ok_or_else(|| JsValue::from_str("no document"))?;

        // alpha:false lets the compositor skip a blend of the whole surface.
        let gl_opts = js_sys::Object::new();
        set_opt(&gl_opts, "alpha", JsValue::FALSE)?;
        set_opt(&gl_opts, "antialias", JsValue::FALSE)?;
        set_opt(&gl_opts, "depth", JsValue::FALSE)?;
        set_opt(&gl_opts, "stencil", JsValue::FALSE)?;
        set_opt(&gl_opts, "premultipliedAlpha", JsValue::FALSE)?;
        set_opt(&gl_opts, "preserveDrawingBuffer", JsValue::FALSE)?;
        set_opt(&gl_opts, "desynchronized", JsValue::TRUE)?;
        let gl = canvas
            .get_context_with_context_options("webgl2", &gl_opts)?
            .ok_or_else(|| JsValue::from_str("webgl2 unavailable"))?
            .dyn_into::<Gl>()?;

        // Detached raster canvas: never appended, so it costs no layout.
        let raster: HtmlCanvasElement = doc.create_element("canvas")?.dyn_into()?;
        let r_opts = js_sys::Object::new();
        // getImageData is the atlas upload path, so opt into the readback-fast
        // (software) backing store for this canvas only.
        set_opt(&r_opts, "willReadFrequently", JsValue::TRUE)?;
        set_opt(&r_opts, "alpha", JsValue::TRUE)?;
        let ctx2d = raster
            .get_context_with_context_options("2d", &r_opts)?
            .ok_or_else(|| JsValue::from_str("2d context unavailable"))?
            .dyn_into::<CanvasRenderingContext2d>()?;

        let program = link_program(&gl, VERT_SRC, FRAG_SRC)?;
        let u_res = gl.get_uniform_location(&program, "u_res");
        let u_tex = gl.get_uniform_location(&program, "u_tex");
        gl.use_program(Some(&program));
        gl.uniform1i(u_tex.as_ref(), 0);

        let vbo = gl
            .create_buffer()
            .ok_or_else(|| JsValue::from_str("create_buffer failed"))?;
        let vao = gl.create_vertex_array();
        gl.bind_vertex_array(vao.as_ref());
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&vbo));
        let stride = (FLOATS_PER_VERT * 4) as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_with_i32(0, 2, Gl::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_with_i32(1, 4, Gl::FLOAT, false, stride, 8);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_with_i32(2, 2, Gl::FLOAT, false, stride, 24);

        let tex = gl
            .create_texture()
            .ok_or_else(|| JsValue::from_str("create_texture failed"))?;

        gl.enable(Gl::BLEND);
        gl.blend_func(Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA);
        gl.disable(Gl::DEPTH_TEST);
        gl.clear_color(BG[0], BG[1], BG[2], 1.0);

        let dpr = if win.device_pixel_ratio() > 0.0 {
            win.device_pixel_ratio()
        } else {
            1.0
        };

        let mut me = TermRenderer {
            canvas,
            gl,
            program,
            u_res,
            vao,
            vbo,
            raster,
            ctx2d,
            family: "ui-monospace, 'Cascadia Mono', 'JetBrains Mono', monospace".to_string(),
            font_px: 14.0,
            dpr,
            metrics: Metrics {
                cell_w: 8,
                cell_h: 16,
                ascent: 12,
                pad: 2,
                box_w: 28,
                cell_w_css: 8.0,
                cell_h_css: 16.0,
            },
            tex,
            atlas_size: 0,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            glyphs: HashMap::new(),
            white_uv: (0.0, 0.0),
            atlas_dirty: false,
            dev_w: 0,
            dev_h: 0,
            bg_verts: Vec::new(),
            fg_verts: Vec::new(),
            staging: Float32Array::new_with_length(FLOATS_PER_QUAD as u32 * 1024),
        };
        me.remeasure()?;
        me.reset_atlas(ATLAS_START)?;
        Ok(me)
    }

    pub fn set_font(&mut self, family: &str, px: f32, dpr: f64) {
        self.family = family.to_string();
        self.font_px = if px.is_finite() && px > 1.0 { px } else { 14.0 };
        self.dpr = if dpr.is_finite() && dpr > 0.0 {
            dpr
        } else {
            1.0
        };
        // Metrics/atlas rebuild failures leave the previous (valid) state in
        // place rather than poisoning the renderer; the next frame retries.
        let _ = self.remeasure();
        let size = self.atlas_size.max(ATLAS_START);
        let _ = self.reset_atlas(size);
    }

    pub fn cell_size_css(&self) -> (f32, f32) {
        (self.metrics.cell_w_css, self.metrics.cell_h_css)
    }

    pub fn resize_to(&mut self, css_w: f64, css_h: f64) {
        let dev_w = (css_w * self.dpr).round().max(1.0) as i32;
        let dev_h = (css_h * self.dpr).round().max(1.0) as i32;
        if self.canvas.width() != dev_w as u32 {
            self.canvas.set_width(dev_w as u32);
        }
        if self.canvas.height() != dev_h as u32 {
            self.canvas.set_height(dev_h as u32);
        }
        let style = self.canvas.style();
        let _ = style.set_property("width", &format!("{css_w}px"));
        let _ = style.set_property("height", &format!("{css_h}px"));
        self.dev_w = dev_w;
        self.dev_h = dev_h;
        self.gl.viewport(0, 0, dev_w, dev_h);
    }

    pub fn grid_for(&self, css_w: f64, css_h: f64) -> (u16, u16) {
        let (cw, ch) = (
            self.metrics.cell_w_css as f64,
            self.metrics.cell_h_css as f64,
        );
        if cw <= 0.0 || ch <= 0.0 {
            return (1, 1);
        }
        let cols = (css_w / cw).floor().clamp(1.0, u16::MAX as f64) as u16;
        let rows = (css_h / ch).floor().clamp(1.0, u16::MAX as f64) as u16;
        (cols, rows)
    }

    pub fn render(&mut self, snap: &GridSnapshot, opts: &RenderOpts) -> f64 {
        let start = now();
        if self.gl.is_context_lost() || self.dev_w == 0 || self.dev_h == 0 {
            return 0.0;
        }

        // A mid-frame atlas grow invalidates every UV emitted so far, so the
        // frame is built again once against the new atlas.
        for _ in 0..2 {
            self.atlas_dirty = false;
            self.build(snap, opts);
            if !self.atlas_dirty {
                break;
            }
        }

        let gl = &self.gl;
        gl.use_program(Some(&self.program));
        gl.bind_vertex_array(self.vao.as_ref());
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo));
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex));
        gl.uniform2f(self.u_res.as_ref(), self.dev_w as f32, self.dev_h as f32);
        gl.clear(Gl::COLOR_BUFFER_BIT);

        // Split borrow: `upload_and_draw` needs &mut self for the staging array.
        let bg = std::mem::take(&mut self.bg_verts);
        self.upload_and_draw(&bg);
        self.bg_verts = bg;
        let fg = std::mem::take(&mut self.fg_verts);
        self.upload_and_draw(&fg);
        self.fg_verts = fg;

        now() - start
    }

    // -- frame construction -------------------------------------------------

    fn build(&mut self, snap: &GridSnapshot, opts: &RenderOpts) {
        self.bg_verts.clear();
        self.fg_verts.clear();

        let cols = snap.cols as usize;
        let rows = snap.rows as usize;
        if cols == 0 || rows == 0 {
            return;
        }
        let m = self.metrics;
        let (sel_lo, sel_hi) = match opts.selection {
            Some((a, b)) if a <= b => (a, b),
            Some((a, b)) => (b, a),
            None => (1, 0), // empty range
        };
        let cur = (snap.cursor_row as usize, snap.cursor_col as usize);
        let cursor_on = opts.focused && opts.cursor_visible;
        let block_cursor = snap.cursor_shape_block;

        for row in 0..rows {
            let y = (row as i32 * m.cell_h) as f32;
            for col in 0..cols {
                let idx = row * cols + col;
                let cell = match snap.cells.get(idx) {
                    Some(c) => c,
                    None => continue,
                };
                let x = (col as i32 * m.cell_w) as f32;

                let mut fg = unpack(cell.fg, FG);
                let mut bg = unpack(cell.bg, BG);
                if cell.dim {
                    fg = [fg[0] * DIM_FACTOR, fg[1] * DIM_FACTOR, fg[2] * DIM_FACTOR];
                }
                if cell.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if idx >= sel_lo && idx <= sel_hi {
                    std::mem::swap(&mut fg, &mut bg);
                }

                let is_cursor = (row, col) == cur;
                let block_fill = is_cursor && cursor_on && block_cursor;
                if block_fill {
                    bg = FLAME;
                    fg = BG;
                }

                if bg != BG {
                    let (u, v) = self.white_uv;
                    push_quad(
                        &mut self.bg_verts,
                        x,
                        y,
                        m.cell_w as f32,
                        m.cell_h as f32,
                        bg,
                        1.0,
                        u,
                        v,
                        u,
                        v,
                    );
                }

                if cell.c != ' ' && cell.c != '\0' {
                    self.emit_glyph(cell.c, cell.bold, cell.italic, x, y, fg);
                }

                if cell.underline {
                    let t = (self.dpr.round() as f32).max(1.0);
                    let uy = y + (m.ascent as f32) + t;
                    let (u, v) = self.white_uv;
                    push_quad(
                        &mut self.fg_verts,
                        x,
                        uy,
                        m.cell_w as f32,
                        t,
                        fg,
                        1.0,
                        u,
                        v,
                        u,
                        v,
                    );
                }
            }
        }

        self.emit_ghost(snap);
        self.emit_cursor_decoration(snap, opts);
    }

    /// Ghost proposals overlay from the cursor cell rightwards, clipped to the
    /// row — they are a preview, not content, and must never reflow the grid.
    fn emit_ghost(&mut self, snap: &GridSnapshot) {
        let ghost = match snap.ghost.as_ref() {
            Some(g) if !g.text.is_empty() => g,
            _ => return,
        };
        let m = self.metrics;
        let cols = snap.cols as usize;
        let row = snap.cursor_row as usize;
        let mut col = snap.cursor_col as usize;
        let y = (row as i32 * m.cell_h) as f32;
        let text: String = ghost.text.chars().take_while(|c| *c != '\n').collect();
        for ch in text.chars() {
            if col >= cols {
                break;
            }
            if ch != ' ' {
                let x = (col as i32 * m.cell_w) as f32;
                self.emit_glyph(ch, false, true, x, y, GHOST);
            }
            col += 1;
        }
    }

    fn emit_cursor_decoration(&mut self, snap: &GridSnapshot, opts: &RenderOpts) {
        let m = self.metrics;
        let x = (snap.cursor_col as i32 * m.cell_w) as f32;
        let y = (snap.cursor_row as i32 * m.cell_h) as f32;
        let (w, h) = (m.cell_w as f32, m.cell_h as f32);
        let (u, v) = self.white_uv;
        let px = (self.dpr.round() as f32).max(1.0);

        if opts.focused {
            if !opts.cursor_visible {
                return;
            }
            if !snap.cursor_shape_block {
                // Block fill was already emitted in the bg pass; only the bar
                // shape needs a foreground rect.
                push_quad(
                    &mut self.fg_verts,
                    x,
                    y,
                    2.0 * px,
                    h,
                    FLAME,
                    1.0,
                    u,
                    v,
                    u,
                    v,
                );
            }
            return;
        }

        // Unfocused panes keep a hollow outline so the caret is still locatable.
        let t = px;
        let mut rect = |rx: f32, ry: f32, rw: f32, rh: f32| {
            push_quad(&mut self.fg_verts, rx, ry, rw, rh, FLAME, 1.0, u, v, u, v);
        };
        rect(x, y, w, t);
        rect(x, y + h - t, w, t);
        rect(x, y, t, h);
        rect(x + w - t, y, t, h);
    }

    fn emit_glyph(&mut self, ch: char, bold: bool, italic: bool, x: f32, y: f32, color: [f32; 3]) {
        let g = match self.glyph(ch, bold, italic) {
            Some(g) => g,
            None => return,
        };
        let m = self.metrics;
        push_quad(
            &mut self.fg_verts,
            x - m.pad as f32,
            y - m.pad as f32,
            g.w,
            g.h,
            color,
            1.0,
            g.u0,
            g.v0,
            g.u1,
            g.v1,
        );
    }

    // -- atlas --------------------------------------------------------------

    fn glyph(&mut self, ch: char, bold: bool, italic: bool) -> Option<Glyph> {
        if let Some(g) = self.glyphs.get(&(ch, bold, italic)) {
            return Some(*g);
        }
        match self.rasterize(ch, bold, italic) {
            Ok(g) => {
                self.glyphs.insert((ch, bold, italic), g);
                Some(g)
            }
            Err(_) => None,
        }
    }

    fn rasterize(&mut self, ch: char, bold: bool, italic: bool) -> Result<Glyph, JsValue> {
        let m = self.metrics;
        self.ctx2d.set_font(&self.font_string(bold, italic));
        self.ctx2d.set_text_baseline("alphabetic");
        self.ctx2d.set_text_align("left");

        let mut buf = [0u8; 4];
        let s: &str = ch.encode_utf8(&mut buf);
        let tm = self.ctx2d.measure_text(s)?;
        let advance = tm.width();
        let right = tm.actual_bounding_box_right();
        let ink = if right.is_finite() {
            advance.max(right)
        } else {
            advance
        };
        let w = (ink.ceil() as i32 + 2 * m.pad).clamp(1, m.box_w);
        let h = m.cell_h + 2 * m.pad;

        self.ctx2d.clear_rect(0.0, 0.0, m.box_w as f64, h as f64);
        // White coverage: the shader reads alpha only, so one raster serves
        // every colour this glyph will ever be drawn in.
        self.ctx2d.set_fill_style_str("#ffffff");
        self.ctx2d
            .fill_text(s, m.pad as f64, (m.pad + m.ascent) as f64)?;

        let (ax, ay) = self.alloc(w, h)?;
        let data = self.ctx2d.get_image_data(0.0, 0.0, w as f64, h as f64)?;
        self.gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex));
        self.gl.tex_sub_image_2d_with_u32_and_u32_and_image_data(
            Gl::TEXTURE_2D,
            0,
            ax,
            ay,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            &data,
        )?;

        let inv = 1.0 / self.atlas_size as f32;
        Ok(Glyph {
            u0: ax as f32 * inv,
            v0: ay as f32 * inv,
            u1: (ax + w) as f32 * inv,
            v1: (ay + h) as f32 * inv,
            w: w as f32,
            h: h as f32,
        })
    }

    /// Shelf allocator: fill a row left to right, then start a new shelf.
    /// On overflow the atlas doubles and every glyph is re-rasterised lazily.
    fn alloc(&mut self, w: i32, h: i32) -> Result<(i32, i32), JsValue> {
        if self.shelf_x + w > self.atlas_size {
            self.shelf_x = 0;
            self.shelf_y += self.shelf_h;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > self.atlas_size {
            let next = self.atlas_size * 2;
            if next > ATLAS_MAX {
                return Err(JsValue::from_str("glyph atlas exhausted"));
            }
            self.reset_atlas(next)?;
            self.atlas_dirty = true;
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Ok((x, y))
    }

    /// (Re)allocate the atlas texture, drop the glyph cache and re-seed the
    /// opaque white texel that solid quads sample.
    fn reset_atlas(&mut self, size: i32) -> Result<(), JsValue> {
        let gl = &self.gl;
        gl.bind_texture(Gl::TEXTURE_2D, Some(&self.tex));
        gl.pixel_storei(Gl::UNPACK_FLIP_Y_WEBGL, 0);
        gl.pixel_storei(Gl::UNPACK_PREMULTIPLY_ALPHA_WEBGL, 0);
        gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA as i32,
            size,
            size,
            0,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            None,
        )?;
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::LINEAR as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
        gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);

        self.atlas_size = size;
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.glyphs.clear();

        // A solid block (not a single texel) so LINEAR filtering can never
        // bleed transparent neighbours into a background rect.
        let block = 8;
        self.ctx2d.clear_rect(0.0, 0.0, block as f64, block as f64);
        self.ctx2d.set_fill_style_str("#ffffff");
        self.ctx2d.fill_rect(0.0, 0.0, block as f64, block as f64);
        let (ax, ay) = self.alloc(block, block)?;
        let data = self
            .ctx2d
            .get_image_data(0.0, 0.0, block as f64, block as f64)?;
        self.gl.tex_sub_image_2d_with_u32_and_u32_and_image_data(
            Gl::TEXTURE_2D,
            0,
            ax,
            ay,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            &data,
        )?;
        let inv = 1.0 / size as f32;
        self.white_uv = (
            (ax as f32 + block as f32 * 0.5) * inv,
            (ay as f32 + block as f32 * 0.5) * inv,
        );
        Ok(())
    }

    // -- metrics ------------------------------------------------------------

    fn font_string(&self, bold: bool, italic: bool) -> String {
        let px = (self.font_px as f64 * self.dpr).max(1.0);
        format!(
            "{}{}{}px {}",
            if italic { "italic " } else { "" },
            if bold { "bold " } else { "" },
            px,
            self.family
        )
    }

    /// Derive integer device-pixel cell geometry from the font itself.
    fn remeasure(&mut self) -> Result<(), JsValue> {
        let px_dev = (self.font_px as f64 * self.dpr).max(1.0);
        self.ctx2d.set_font(&self.font_string(false, false));
        self.ctx2d.set_text_baseline("alphabetic");
        let tm = self.ctx2d.measure_text("M")?;

        let cell_w = tm.width().max(1.0).ceil() as i32;
        // fontBoundingBox* is the authoritative line box where implemented;
        // Safari <16.4 and older engines omit it, yielding NaN.
        let a = tm.font_bounding_box_ascent();
        let d = tm.font_bounding_box_descent();
        let (cell_h, ascent) = if a.is_finite() && d.is_finite() && a > 0.0 && (a + d) > 1.0 {
            ((a + d).ceil() as i32, a.ceil() as i32)
        } else {
            let h = (px_dev * 1.4).ceil() as i32;
            (h, (px_dev * 1.1).round() as i32)
        };

        let pad = ((self.dpr * 2.0).ceil() as i32).max(2);
        self.metrics = Metrics {
            cell_w,
            cell_h,
            ascent: ascent.min(cell_h),
            pad,
            // Room for double-width and overhanging glyphs plus both pads.
            box_w: cell_w * 3 + 2 * pad,
            cell_w_css: (cell_w as f64 / self.dpr) as f32,
            cell_h_css: (cell_h as f64 / self.dpr) as f32,
        };

        // Resizing the raster canvas also resets its 2d state, so font/baseline
        // are re-applied by every rasterize() call rather than cached here.
        let bh = (self.metrics.cell_h + 2 * pad).max(1) as u32;
        if self.raster.width() != self.metrics.box_w as u32 {
            self.raster.set_width(self.metrics.box_w.max(1) as u32);
        }
        if self.raster.height() != bh {
            self.raster.set_height(bh);
        }
        self.ctx2d.set_image_smoothing_enabled(false);
        Ok(())
    }

    // -- upload -------------------------------------------------------------

    fn upload_and_draw(&mut self, verts: &[f32]) {
        if verts.is_empty() {
            return;
        }
        let n = verts.len() as u32;
        if self.staging.length() < n {
            self.staging = Float32Array::new_with_length(n.next_power_of_two());
        }
        let view = self.staging.subarray(0, n);
        view.copy_from(verts);
        self.gl.buffer_data_with_array_buffer_view(
            Gl::ARRAY_BUFFER,
            view.as_ref(),
            Gl::DYNAMIC_DRAW,
        );
        self.gl
            .draw_arrays(Gl::TRIANGLES, 0, (verts.len() / FLOATS_PER_VERT) as i32);
    }
}

/// Two triangles, six vertices, interleaved pos/colour/uv.
#[allow(clippy::too_many_arguments)]
fn push_quad(
    out: &mut Vec<f32>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    c: [f32; 3],
    a: f32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let (x1, y1) = (x + w, y + h);
    let corners = [
        (x, y, u0, v0),
        (x1, y, u1, v0),
        (x, y1, u0, v1),
        (x1, y, u1, v0),
        (x1, y1, u1, v1),
        (x, y1, u0, v1),
    ];
    out.reserve(FLOATS_PER_QUAD);
    for (vx, vy, u, v) in corners {
        out.extend_from_slice(&[vx, vy, c[0], c[1], c[2], a, u, v]);
    }
}

fn set_opt(obj: &js_sys::Object, key: &str, val: JsValue) -> Result<(), JsValue> {
    js_sys::Reflect::set(obj, &JsValue::from_str(key), &val).map(|_| ())
}

fn now() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn compile(gl: &Gl, kind: u32, src: &str) -> Result<web_sys::WebGlShader, JsValue> {
    let sh = gl
        .create_shader(kind)
        .ok_or_else(|| JsValue::from_str("create_shader failed"))?;
    gl.shader_source(&sh, src);
    gl.compile_shader(&sh);
    let ok = gl
        .get_shader_parameter(&sh, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false);
    if ok {
        Ok(sh)
    } else {
        let log = gl.get_shader_info_log(&sh).unwrap_or_default();
        gl.delete_shader(Some(&sh));
        Err(JsValue::from_str(&format!("shader compile failed: {log}")))
    }
}

fn link_program(gl: &Gl, vs: &str, fs: &str) -> Result<WebGlProgram, JsValue> {
    let v = compile(gl, Gl::VERTEX_SHADER, vs)?;
    let f = compile(gl, Gl::FRAGMENT_SHADER, fs)?;
    let p = gl
        .create_program()
        .ok_or_else(|| JsValue::from_str("create_program failed"))?;
    gl.attach_shader(&p, &v);
    gl.attach_shader(&p, &f);
    gl.link_program(&p);
    // Shaders are refcounted by the program; drop our references either way.
    gl.delete_shader(Some(&v));
    gl.delete_shader(Some(&f));
    let ok = gl
        .get_program_parameter(&p, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false);
    if ok {
        Ok(p)
    } else {
        let log = gl.get_program_info_log(&p).unwrap_or_default();
        Err(JsValue::from_str(&format!("program link failed: {log}")))
    }
}
