// Draws the "capsule" overlay with GDI+ (part of Windows): a rounded pill with signal bars,
// ping, NET and MAX, plus optional pills underneath for the gateway address and recent problems.

use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Graphics::Gdi::HBITMAP;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;
use windows_sys::core::GUID;

use crate::util::wide;

const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x0026200A;
const FONT_STYLE_REGULAR: i32 = 0;
/// CLSID of the built-in PNG encoder.
const PNG_ENCODER: GUID = GUID::from_u128(0x557cf406_1a04_11d3_9a73_0000f81ef32e);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Color(pub u32);

impl Color {
    pub const fn argb(a: u8, r: u8, g: u8, b: u8) -> Color {
        Color((a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::argb(255, r, g, b)
    }
}

const PILL_FILL: Color = Color::argb(204, 14, 16, 20);
const HAIRLINE: Color = Color::argb(18, 255, 255, 255);
const DIVIDER: Color = Color::argb(31, 255, 255, 255);
const TEXT: Color = Color::rgb(238, 242, 246);
const MUTED: Color = Color::rgb(154, 161, 171);
const LABEL: Color = Color::rgb(141, 148, 158);
const VALUE: Color = Color::rgb(201, 206, 214);
const HOT_VALUE: Color = Color::rgb(255, 138, 128);
const ALARM_LINE: Color = Color::rgb(255, 90, 80);
const MOVE_LINE: Color = Color::rgb(120, 170, 255);
const ALARM_FILL: Color = Color::argb(230, 84, 18, 16);
const ALARM_TEXT: Color = Color::rgb(255, 194, 188);
const WARN_TEXT: Color = Color::rgb(255, 191, 115);
const WARN_ICON: Color = Color::rgb(255, 170, 60);
const WHITE: Color = Color::rgb(255, 255, 255);

/// Everything the overlay shows; a new picture is drawn only when this changes.
#[derive(Clone, PartialEq, Debug)]
pub struct OverlayModel {
    pub quality: Color,
    pub ping: String,
    pub network: Option<String>,
    pub max: Option<String>,
    pub max_hot: bool,
    pub status: Option<String>,
    pub alert_head: Option<String>,
    pub alert_tail: Option<String>,
    pub flash: bool,
    pub server: Option<String>,
    pub unlocked: bool,
}

/// GDI+ for the lifetime of this value.
pub struct GdiPlus(usize);

impl GdiPlus {
    pub fn start() -> GdiPlus {
        let mut token = 0usize;
        let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
        unsafe { GdiplusStartup(&mut token, &input, null_mut()) };
        GdiPlus(token)
    }
}

impl Drop for GdiPlus {
    fn drop(&mut self) {
        unsafe { GdiplusShutdown(self.0) };
    }
}

// ── thin RAII wrappers ──

struct Bitmap(*mut GpBitmap);

impl Bitmap {
    fn new(w: i32, h: i32) -> Bitmap {
        let mut b = null_mut();
        unsafe { GdipCreateBitmapFromScan0(w, h, 0, PIXEL_FORMAT_32BPP_ARGB, null(), &mut b) };
        Bitmap(b)
    }
    fn image(&self) -> *mut GpImage {
        self.0.cast()
    }
    fn graphics(&self) -> Graphics {
        let mut g = null_mut();
        unsafe { GdipGetImageGraphicsContext(self.image(), &mut g) };
        Graphics(g)
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GdipDisposeImage(self.image()) };
        }
    }
}

struct Graphics(*mut GpGraphics);

impl Drop for Graphics {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GdipDeleteGraphics(self.0) };
        }
    }
}

struct Brush(*mut GpSolidFill);

impl Brush {
    fn new(c: Color) -> Brush {
        let mut b = null_mut();
        unsafe { GdipCreateSolidFill(c.0, &mut b) };
        Brush(b)
    }
    fn ptr(&self) -> *mut GpBrush {
        self.0.cast()
    }
}

impl Drop for Brush {
    fn drop(&mut self) {
        unsafe { GdipDeleteBrush(self.ptr()) };
    }
}

struct Pen(*mut GpPen);

impl Pen {
    fn new(c: Color, width: f32) -> Pen {
        let mut p = null_mut();
        unsafe { GdipCreatePen1(c.0, width, UnitWorld, &mut p) };
        Pen(p)
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { GdipDeletePen(self.0) };
    }
}

struct Family {
    ptr: *mut GpFontFamily,
    /// cell ascent / em height
    ascent_ratio: f32,
}

impl Family {
    fn open(name: &str) -> Option<Family> {
        let mut f = null_mut();
        let w = wide(name);
        if unsafe { GdipCreateFontFamilyFromName(w.as_ptr(), null_mut(), &mut f) } != 0 || f.is_null() {
            return None;
        }
        let (mut ascent, mut em) = (0u16, 0u16);
        unsafe {
            GdipGetCellAscent(f, FONT_STYLE_REGULAR, &mut ascent);
            GdipGetEmHeight(f, FONT_STYLE_REGULAR, &mut em);
        }
        Some(Family { ptr: f, ascent_ratio: if em > 0 { ascent as f32 / em as f32 } else { 0.8 } })
    }

    fn pick(wanted: &str, fallback: &str) -> Family {
        Family::open(wanted).or_else(|| Family::open(fallback)).or_else(|| Family::open("Arial")).expect("no usable font")
    }
}

impl Drop for Family {
    fn drop(&mut self) {
        unsafe { GdipDeleteFontFamily(self.ptr) };
    }
}

struct Font {
    ptr: *mut GpFont,
    size: f32,
    ascent_ratio: f32,
}

impl Font {
    fn new(family: &Family, px: f32) -> Font {
        let mut f = null_mut();
        unsafe { GdipCreateFont(family.ptr, px, FONT_STYLE_REGULAR, UnitPixel, &mut f) };
        Font { ptr: f, size: px, ascent_ratio: family.ascent_ratio }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        unsafe { GdipDeleteFont(self.ptr) };
    }
}

struct Fonts {
    big: Font,
    unit: Font,
    label: Font,
    value: Font,
    note: Font,
    note_bold: Font,
    small: Font,
}

/// Holds the font families; create after GdiPlus::start() and drop before it.
pub struct Renderer {
    regular: Family,
    bold: Family,
    format: *mut GpStringFormat,
}

impl Renderer {
    pub fn new() -> Renderer {
        let mut format = null_mut();
        unsafe { GdipStringFormatGetGenericTypographic(&mut format) };
        Renderer {
            regular: Family::pick("Bahnschrift", "Segoe UI"),
            bold: Family::pick("Bahnschrift SemiBold", "Segoe UI Semibold"),
            format,
        }
    }

    fn fonts(&self, s: f32) -> Fonts {
        Fonts {
            big: Font::new(&self.bold, 22.0 * s),
            unit: Font::new(&self.regular, 11.0 * s),
            label: Font::new(&self.regular, 10.0 * s),
            value: Font::new(&self.regular, 13.0 * s),
            note: Font::new(&self.regular, 12.0 * s),
            note_bold: Font::new(&self.bold, 12.0 * s),
            small: Font::new(&self.regular, 11.0 * s),
        }
    }

    fn measure(&self, g: &Graphics, text: &str, f: &Font) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let w: Vec<u16> = text.encode_utf16().collect();
        let layout = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
        let mut bounds = RectF { X: 0.0, Y: 0.0, Width: 0.0, Height: 0.0 };
        unsafe {
            GdipMeasureString(g.0, w.as_ptr(), w.len() as i32, f.ptr, &layout, self.format, &mut bounds, null_mut(), null_mut());
        }
        bounds.Width
    }

    /// Draws text with its baseline at y; returns the advance width.
    fn draw(&self, g: &Graphics, text: &str, f: &Font, c: Color, x: f32, baseline: f32) -> f32 {
        let ascent = f.size * f.ascent_ratio;
        let w: Vec<u16> = text.encode_utf16().collect();
        let layout = RectF { X: x, Y: baseline - ascent, Width: 0.0, Height: 0.0 };
        let b = Brush::new(c);
        unsafe { GdipDrawString(g.0, w.as_ptr(), w.len() as i32, f.ptr, &layout, self.format, b.ptr()) };
        self.measure(g, text, f)
    }

    fn render(&self, m: &OverlayModel, s: f32) -> Bitmap {
        let f = self.fonts(s);
        let probe = Bitmap::new(1, 1);
        let mg = probe.graphics();
        let w = |t: &str, font: &Font| self.measure(&mg, t, font);

        // ── layout ──
        let (cap_h, gap) = (34.0 * s, 10.0 * s);
        let mut cap_w = 12.0 * s + 16.0 * s + gap + w(&m.ping, &f.big) + 3.0 * s + w("ms", &f.unit) + gap + 1.0 * s + gap;
        match &m.status {
            Some(st) => cap_w += w(st, &f.note),
            None => {
                cap_w += w("NET", &f.label) + 4.0 * s + w(opt(&m.network), &f.value) + gap + w("MAX", &f.label) + 4.0 * s
                    + w(opt(&m.max), &f.value)
            }
        }
        cap_w += 16.0 * s;

        let (pill_h, pill_gap) = (25.0 * s, 4.0 * s);
        let server_w = m.server.as_deref().map_or(0.0, |sv| 11.0 * s + w(sv, &f.small) + 14.0 * s);
        let head_font = if m.flash { &f.note_bold } else { &f.note };
        let alert_w = m.alert_head.as_deref().map_or(0.0, |head| {
            11.0 * s + 13.0 * s + 7.0 * s + w(head, head_font) + w(&format!(" {}", opt(&m.alert_tail)), &f.note) + 14.0 * s
        });

        let width = cap_w.max(server_w.max(alert_w)).ceil() as i32 + 2;
        let h = cap_h
            + if m.server.is_some() { pill_gap + pill_h } else { 0.0 }
            + if m.alert_head.is_some() { pill_gap + pill_h } else { 0.0 };
        let bmp = Bitmap::new(width, h.ceil() as i32 + 2);

        let g = bmp.graphics();
        unsafe {
            GdipSetSmoothingMode(g.0, SmoothingModeAntiAlias);
            GdipSetTextRenderingHint(g.0, TextRenderingHintAntiAliasGridFit);
            GdipGraphicsClear(g.0, 0);
        }

        // ── main capsule ──
        let border = if m.unlocked { MOVE_LINE } else if m.flash { ALARM_LINE } else { HAIRLINE };
        pill(&g, 0.5, 0.5, cap_w, cap_h, PILL_FILL, Some(border), s);

        let mut x = 12.0 * s;
        let mid = 0.5 + cap_h / 2.0;
        let baseline = mid + 22.0 * s * 0.36;
        {
            let q = Brush::new(m.quality);
            for (i, hs) in [4.0f32, 7.0, 10.0, 14.0].into_iter().enumerate() {
                unsafe { GdipFillRectangle(g.0, q.ptr(), x + i as f32 * 4.3 * s, mid + 7.0 * s - hs * s, 3.0 * s, hs * s) };
            }
        }
        x += 16.0 * s + gap;
        x += self.draw(&g, &m.ping, &f.big, TEXT, x, baseline) + 3.0 * s;
        x += self.draw(&g, "ms", &f.unit, MUTED, x, baseline) + gap;
        {
            let d = Brush::new(DIVIDER);
            unsafe { GdipFillRectangle(g.0, d.ptr(), x, mid - 8.0 * s, 1.0 * s, 16.0 * s) };
        }
        x += 1.0 * s + gap;
        match &m.status {
            Some(st) => {
                self.draw(&g, st, &f.note, MUTED, x, baseline);
            }
            None => {
                x += self.draw(&g, "NET", &f.label, LABEL, x, baseline) + 4.0 * s;
                x += self.draw(&g, opt(&m.network), &f.value, VALUE, x, baseline) + gap;
                x += self.draw(&g, "MAX", &f.label, LABEL, x, baseline) + 4.0 * s;
                self.draw(&g, opt(&m.max), &f.value, if m.max_hot { HOT_VALUE } else { VALUE }, x, baseline);
            }
        }

        // ── extra pills ──
        let mut y = 0.5 + cap_h;
        if let Some(server) = &m.server {
            y += pill_gap;
            pill(&g, 0.5, y, server_w, pill_h, PILL_FILL, None, s);
            self.draw(&g, server, &f.small, MUTED, 0.5 + 11.0 * s, y + pill_h / 2.0 + 11.0 * s * 0.36);
            y += pill_h;
        }
        if let Some(head) = &m.alert_head {
            y += pill_gap;
            pill(&g, 0.5, y, alert_w, pill_h, if m.flash { ALARM_FILL } else { PILL_FILL }, None, s);
            let mut ax = 0.5 + 11.0 * s;
            let am = y + pill_h / 2.0;
            let ab = am + 12.0 * s * 0.36;
            warning(&g, ax, am, if m.flash { ALARM_LINE } else { WARN_ICON }, s);
            ax += 13.0 * s + 7.0 * s;
            ax += self.draw(&g, head, head_font, if m.flash { WHITE } else { WARN_TEXT }, ax, ab);
            let tail = format!(" {}", opt(&m.alert_tail));
            self.draw(&g, &tail, &f.note, if m.flash { ALARM_TEXT } else { WARN_TEXT }, ax, ab);
        }
        drop(g);
        bmp
    }

    /// Renders the overlay into a GDI bitmap (premultiplied alpha) for UpdateLayeredWindow.
    /// The caller deletes the HBITMAP.
    pub fn render_hbitmap(&self, m: &OverlayModel, scale: f32) -> Option<(HBITMAP, i32, i32)> {
        let bmp = self.render(m, scale);
        let (mut w, mut h) = (0u32, 0u32);
        let mut hbmp: HBITMAP = null_mut();
        unsafe {
            GdipGetImageWidth(bmp.image(), &mut w);
            GdipGetImageHeight(bmp.image(), &mut h);
            if GdipCreateHBITMAPFromBitmap(bmp.0, &mut hbmp, 0) != 0 {
                return None;
            }
        }
        Some((hbmp, w as i32, h as i32))
    }

    /// Renders the overlay on a solid background with padding and saves it as PNG.
    pub fn save_demo_png(&self, m: &OverlayModel, scale: f32, pad: i32, background: Color, path: &Path) -> bool {
        let pill = self.render(m, scale);
        let (mut w, mut h) = (0u32, 0u32);
        unsafe {
            GdipGetImageWidth(pill.image(), &mut w);
            GdipGetImageHeight(pill.image(), &mut h);
        }
        let shot = Bitmap::new(w as i32 + pad * 2, h as i32 + pad * 2);
        {
            let g = shot.graphics();
            unsafe {
                GdipGraphicsClear(g.0, background.0);
                GdipDrawImageRectI(g.0, pill.image(), pad, pad, w as i32, h as i32);
            }
        }
        let file = wide(&path.to_string_lossy());
        unsafe { GdipSaveImageToFile(shot.image(), file.as_ptr(), &PNG_ENCODER, null()) == 0 }
    }
}

fn opt(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("")
}

#[allow(clippy::too_many_arguments)]
fn pill(g: &Graphics, x: f32, y: f32, w: f32, h: f32, fill: Color, line: Option<Color>, s: f32) {
    unsafe {
        let mut path = null_mut();
        GdipCreatePath(FillModeAlternate, &mut path);
        GdipAddPathArc(path, x, y, h, h, 90.0, 180.0);
        GdipAddPathArc(path, x + w - h, y, h, h, 270.0, 180.0);
        GdipClosePathFigure(path);
        let b = Brush::new(fill);
        GdipFillPath(g.0, b.ptr(), path);
        if let Some(line) = line {
            let p = Pen::new(line, 1.0 * s);
            GdipDrawPath(g.0, p.0, path);
        }
        GdipDeletePath(path);
    }
}

/// A small outlined warning triangle with an exclamation mark, centred vertically on mid.
fn warning(g: &Graphics, x: f32, mid: f32, c: Color, s: f32) {
    let top = mid - 5.0 * s;
    let pen = Pen::new(c, 1.6 * s);
    let pts = [
        PointF { X: x + 6.5 * s, Y: top },
        PointF { X: x + 12.0 * s, Y: top + 10.0 * s },
        PointF { X: x + 1.0 * s, Y: top + 10.0 * s },
    ];
    let b = Brush::new(c);
    unsafe {
        GdipSetPenLineJoin(pen.0, LineJoinRound);
        GdipSetPenStartCap(pen.0, LineCapRound);
        GdipSetPenEndCap(pen.0, LineCapRound);
        GdipDrawPolygon(g.0, pen.0, pts.as_ptr(), 3);
        GdipDrawLine(g.0, pen.0, x + 6.5 * s, top + 3.5 * s, x + 6.5 * s, top + 6.3 * s);
        GdipFillEllipse(g.0, b.ptr(), x + 5.6 * s, top + 7.3 * s, 1.8 * s, 1.8 * s);
    }
}

/// Tray icon — signal bars in the given colour. The caller owns (and may cache) the icon.
pub fn tray_icon(color: Color, size: i32) -> HICON {
    let bmp = Bitmap::new(size, size);
    {
        let g = bmp.graphics();
        let b = Brush::new(color);
        let sz = size as f32;
        let (w, gap) = (sz * 0.17, sz * 0.07);
        let left = (sz - (4.0 * w + 3.0 * gap)) / 2.0;
        let bottom = sz * 0.92;
        unsafe {
            GdipSetSmoothingMode(g.0, SmoothingModeAntiAlias);
            GdipGraphicsClear(g.0, 0);
            for (i, hh) in [0.30f32, 0.50, 0.70, 0.90].into_iter().enumerate() {
                GdipFillRectangle(g.0, b.ptr(), left + i as f32 * (w + gap), bottom - sz * hh, w, sz * hh);
            }
        }
    }
    let mut icon: HICON = null_mut();
    unsafe { GdipCreateHICONFromBitmap(bmp.0, &mut icon) };
    icon
}
