use windows::Win32::Foundation::{COLORREF, POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::core::w;

use crate::palette::{self, Rgb, TrayTheme};

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub(super) scale: f32,
    pub(super) theme: TrayTheme,
}

impl Layout {
    pub(super) fn colors(self) -> palette::DashboardColors {
        self.theme.dashboard()
    }

    pub(super) fn px(self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    pub(super) fn rect(self, left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left: self.px(left),
            top: self.px(top),
            right: self.px(right),
            bottom: self.px(bottom),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Font {
    size: i32,
    weight: FONT_WEIGHT,
    strikeout: bool,
}

impl Font {
    pub(super) const fn regular(size: i32) -> Self {
        Self {
            size,
            weight: FW_NORMAL,
            strikeout: false,
        }
    }

    pub(super) const fn semibold(size: i32) -> Self {
        Self {
            size,
            weight: FW_SEMIBOLD,
            strikeout: false,
        }
    }

    pub(super) const fn bold(size: i32) -> Self {
        Self {
            size,
            weight: FW_BOLD,
            strikeout: false,
        }
    }

    pub(super) const fn strikeout(self) -> Self {
        Self {
            strikeout: true,
            ..self
        }
    }
}

pub(super) fn create_font(layout: Layout, font: Font) -> HFONT {
    unsafe {
        CreateFontW(
            -layout.px(font.size).max(1),
            0,
            0,
            0,
            font.weight.0 as i32,
            0,
            0,
            u32::from(font.strikeout),
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        )
    }
}

pub(super) fn draw_text(
    dc: HDC,
    layout: Layout,
    mut rect: RECT,
    value: &str,
    font: Font,
    color: Rgb,
    alignment: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let font = create_font(layout, font);
        let old = SelectObject(dc, font.into());
        SetTextColor(dc, colorref(color));
        let mut text = value.encode_utf16().collect::<Vec<_>>();
        DrawTextW(dc, &mut text, &mut rect, alignment | DT_NOPREFIX);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
    }
}

pub(super) fn measure_text(dc: HDC, layout: Layout, value: &str, font: Font) -> i32 {
    unsafe {
        let font = create_font(layout, font);
        let old = SelectObject(dc, font.into());
        let mut text = value.encode_utf16().collect::<Vec<_>>();
        let mut rect = RECT::default();
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        ((rect.right - rect.left) as f32 / layout.scale).ceil() as i32
    }
}

pub(super) fn fill_rect(dc: HDC, rect: RECT, color: Rgb) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

pub(super) fn fill_round_rect(dc: HDC, rect: RECT, radius: i32, color: Rgb) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        let old_brush = SelectObject(dc, brush.into());
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let _ = RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius,
            radius,
        );
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(brush.into());
    }
}

pub(super) fn fill_ellipse(dc: HDC, rect: RECT, color: Rgb) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        let old_brush = SelectObject(dc, brush.into());
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let _ = Ellipse(dc, rect.left, rect.top, rect.right + 1, rect.bottom + 1);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(brush.into());
    }
}

pub(super) fn draw_polyline(dc: HDC, points: &[POINT], color: Rgb, width: i32) {
    if points.len() < 2 {
        return;
    }
    unsafe {
        let pen = if width <= 1 {
            CreatePen(PS_SOLID, 1, colorref(color))
        } else {
            let brush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: colorref(color),
                lbHatch: 0,
            };
            ExtCreatePen(
                PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
                width as u32,
                &brush,
                None,
            )
        };
        let old = SelectObject(dc, pen.into());
        let _ = Polyline(dc, points);
        SelectObject(dc, old);
        let _ = DeleteObject(pen.into());
    }
}

pub(super) fn colorref(Rgb(red, green, blue): Rgb) -> COLORREF {
    COLORREF(red as u32 | ((green as u32) << 8) | ((blue as u32) << 16))
}

pub(super) fn bgra(Rgb(red, green, blue): Rgb) -> u32 {
    ((255u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}
