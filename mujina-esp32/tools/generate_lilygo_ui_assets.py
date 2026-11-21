"""Generate 4x-supersampled Nano 3S-styled 320x170 RGB565 templates and 8-bit alpha fonts for LilyGO T-Display-S3."""

import argparse
import math
import os
from pathlib import Path

import numpy as np
import pymupdf
from PIL import Image, ImageDraw, ImageFilter, ImageFont

TOOLS_DIR = Path(__file__).resolve().parent
ESP32_DIR = TOOLS_DIR.parent
REPO_ROOT = ESP32_DIR.parent
SVG_PATH = str(TOOLS_DIR / "assets" / "mujina-head-mark.svg")
DEFAULT_ASSETS_RS = ESP32_DIR / "src" / "ui_assets.rs"
DEFAULT_FONTS_RS = ESP32_DIR / "src" / "ui_fonts.rs"

W = 320
H = 170
SCALE = 4
CW = W * SCALE  # 1280
CH = H * SCALE  # 680

FONT_DIRS = [
    str(TOOLS_DIR / "fonts"),
    r"D:\github\nexus-miner-app\assets\fonts",
    r"C:\Windows\Fonts",
]


def _find_font(name: str) -> str:
    for d in FONT_DIRS:
        p = os.path.join(d, name)
        if os.path.exists(p):
            return p
    raise FileNotFoundError(f"Missing required font: {name}")


ORBITRON = _find_font("Orbitron-Bold.ttf")
SPACE_MONO = _find_font("SpaceMono-Regular.ttf")
BAHNSCHRIFT = _find_font("bahnschrift.ttf")
SEGOE_BOLD = _find_font("segoeuib.ttf")
COURIER_BOLD = _find_font("courbd.ttf")


def get_font(path: str, size: float) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(path, max(1, int(round(size))))


def lerp_color(c1, c2, t: float):
    t = max(0.0, min(1.0, t))
    return tuple(int(c1[i] + (c2[i] - c1[i]) * t) for i in range(len(c1)))


def draw_centered_text(draw: ImageDraw.ImageDraw, cx: float, y: float, text: str, font, fill):
    bbox = draw.textbbox((0, 0), text, font=font)
    tw = bbox[2] - bbox[0]
    draw.text((cx - tw / 2.0, y), text, font=font, fill=fill)
    return tw, bbox[3] - bbox[1]


def draw_smooth_arc(draw: ImageDraw.ImageDraw, cx: float, cy: float, r_mid: float, start_deg: float, end_deg: float, width: float, color, round_caps: bool = True):
    sweep = end_deg - start_deg
    if abs(sweep) < 0.01:
        return
    if not round_caps:
        steps = max(8, int(abs(sweep) * 1.5))
        r_out = r_mid + width / 2.0
        r_in = r_mid - width / 2.0
        outer_pts, inner_pts = [], []
        for i in range(steps + 1):
            ang = math.radians(start_deg + sweep * (i / steps))
            ca, sa = math.cos(ang), math.sin(ang)
            outer_pts.append((cx + r_out * ca, cy + r_out * sa))
            inner_pts.append((cx + r_in * ca, cy + r_in * sa))
        draw.polygon(outer_pts + list(reversed(inner_pts)), fill=color)
        return

    steps = max(24, int(abs(sweep) * 2.5))
    rad = width / 2.0
    for i in range(steps + 1):
        t = i / steps
        ang = math.radians(start_deg + sweep * t)
        px = cx + r_mid * math.cos(ang)
        py = cy + r_mid * math.sin(ang)
        draw.ellipse([px - rad, py - rad, px + rad, py + rad], fill=color)


def draw_chamfered_card(draw: ImageDraw.ImageDraw, x: float, y: float, w: float, h: float, chamfer: float, fill, outline, outline_w: float, left_neon=None):
    pts = [
        (x, y),
        (x + w - chamfer, y),
        (x + w, y + chamfer),
        (x + w, y + h),
        (x + chamfer, y + h),
        (x, y + h - chamfer),
    ]
    draw.polygon(pts, fill=fill)
    for i in range(len(pts)):
        draw.line([pts[i], pts[(i + 1) % len(pts)]], fill=outline, width=max(1, int(round(outline_w))))
    if left_neon is not None:
        draw.line([(x + 1.5 * SCALE, y + 2 * SCALE), (x + 1.5 * SCALE, y + h - chamfer - 1 * SCALE)], fill=left_neon, width=int(2.6 * SCALE))


def draw_glass_card(scr: Image.Image, x: float, y: float, w: float, h: float, radius: float, fill, rim, shadow_col, rim_w: float = 1.2):
    layer = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    ld = ImageDraw.Draw(layer, "RGBA")
    ld.rounded_rectangle([x, y + 2.2 * SCALE, x + w, y + h + 2.2 * SCALE], radius=radius, fill=shadow_col)
    ld.rounded_rectangle([x, y, x + w, y + h], radius=radius, fill=fill, outline=rim, width=max(1, int(round(rim_w * SCALE))))
    scr.alpha_composite(layer)


def draw_cyberpunk_bg(scr: Image.Image):
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW, CH], fill=(5, 6, 12, 255))
    step = 18 * SCALE
    grid_col = (0, 240, 255, 18)
    for gx in range((CW // 2) % step, CW, step):
        d.line([(gx, 0), (gx, CH)], fill=grid_col, width=max(1, SCALE // 2))
    for gy in range((CH // 2) % step, CH, step):
        d.line([(0, gy), (CW, gy)], fill=grid_col, width=max(1, SCALE // 2))

    glow = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow, "RGBA")
    gd.ellipse([CW // 2 - 110 * SCALE, CH // 2 - 80 * SCALE, CW // 2 + 110 * SCALE, CH // 2 + 80 * SCALE], fill=(0, 240, 255, 22))
    glow = glow.filter(ImageFilter.GaussianBlur(radius=24 * SCALE))
    scr.alpha_composite(glow)

    sd = ImageDraw.Draw(scr, "RGBA")
    for y in range(0, CH, 3 * SCALE):
        sd.line([(0, y), (CW, y)], fill=(0, 0, 0, 48), width=max(1, SCALE))


def draw_minimal_bg(scr: Image.Image):
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW, CH], fill=(216, 232, 252, 255))
    blobs = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    bd = ImageDraw.Draw(blobs, "RGBA")
    bd.ellipse([-30 * SCALE, -40 * SCALE, 190 * SCALE, 120 * SCALE], fill=(168, 212, 255, 225))
    bd.ellipse([130 * SCALE, -30 * SCALE, 340 * SCALE, 140 * SCALE], fill=(218, 198, 250, 220))
    bd.ellipse([70 * SCALE, 65 * SCALE, 310 * SCALE, 210 * SCALE], fill=(250, 218, 232, 225))
    blobs = blobs.filter(ImageFilter.GaussianBlur(radius=28 * SCALE))
    scr.alpha_composite(blobs)


_RACCOON_CACHE = None


def load_mujina_raccoon() -> Image.Image:
    global _RACCOON_CACHE
    if _RACCOON_CACHE is not None:
        return _RACCOON_CACHE
    doc = pymupdf.open(SVG_PATH)
    pix = doc[0].get_pixmap(alpha=True, dpi=220)
    img = Image.frombytes("RGBA", (pix.width, pix.height), pix.samples)
    bbox = img.getbbox()
    if bbox:
        img = img.crop(bbox)
    _RACCOON_CACHE = img
    return img


def resize_raccoon(target_width: float) -> Image.Image:
    src = load_mujina_raccoon()
    aspect = src.height / float(src.width)
    th = max(1, int(round(target_width * aspect)))
    return src.resize((int(round(target_width)), th), Image.Resampling.LANCZOS)


def make_raccoon_cyberpunk(target_width: float):
    sprite = resize_raccoon(target_width)
    pad = 14 * SCALE
    cw, ch = sprite.width + 2 * pad, sprite.height + 2 * pad
    canvas = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    alpha = sprite.split()[3]
    mask_pad = Image.new("L", (cw, ch), 0)
    mask_pad.paste(alpha, (pad, pad))

    outer_glow = Image.composite(
        Image.new("RGBA", (cw, ch), (0, 240, 255, 220)),
        Image.new("RGBA", (cw, ch), (0, 0, 0, 0)),
        mask_pad,
    ).filter(ImageFilter.GaussianBlur(radius=5.5 * SCALE))
    inner_glow = Image.composite(
        Image.new("RGBA", (cw, ch), (0, 240, 255, 240)),
        Image.new("RGBA", (cw, ch), (0, 0, 0, 0)),
        mask_pad,
    ).filter(ImageFilter.GaussianBlur(radius=2.0 * SCALE))
    canvas.alpha_composite(outer_glow)
    canvas.alpha_composite(inner_glow)

    sprite_pad = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    sprite_pad.paste(sprite, (pad, pad), sprite)
    canvas.alpha_composite(sprite_pad)

    scan = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    sd = ImageDraw.Draw(scan, "RGBA")
    for y in range(0, ch, 3 * SCALE):
        sd.line([(0, y), (cw, y)], fill=(0, 0, 0, 32), width=max(1, SCALE // 2))
    scan.putalpha(Image.composite(scan.split()[3], Image.new("L", (cw, ch), 0), mask_pad))
    canvas.alpha_composite(scan)
    return canvas


def make_raccoon_parasite(target_width: float):
    sprite = resize_raccoon(target_width)
    arr = np.array(sprite, dtype=np.uint8)
    r, g, b, a = arr[:, :, 0].astype(np.float32), arr[:, :, 1].astype(np.float32), arr[:, :, 2].astype(np.float32), arr[:, :, 3]
    lum = 0.299 * r + 0.587 * g + 0.114 * b
    is_vis = a > 55
    is_red = is_vis & (r > 120) & (r > 1.55 * g) & (r > 1.55 * b)

    out = np.zeros_like(arr)
    out[:, :, 3] = a
    high = is_vis & (~is_red) & (lum >= 112)
    mid = is_vis & (~is_red) & (lum >= 62) & (lum < 112)
    dark = is_vis & (~is_red) & (lum < 62)

    out[high, 0:3] = 255
    yy, xx = np.indices((arr.shape[0], arr.shape[1]))
    bayer = (((yy // SCALE) + (xx // SCALE)) % 2) == 0
    out[mid & bayer, 0:3] = 225
    out[mid & (~bayer), 0:3] = 40
    out[dark, 0:3] = 8
    out[is_red, 0:3] = [255, 48, 48]

    mono = Image.fromarray(out, mode="RGBA")
    pad = 5 * SCALE
    cw, ch = mono.width + 2 * pad, mono.height + 2 * pad
    canvas = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    mask_pad = Image.new("L", (cw, ch), 0)
    mask_pad.paste(mono.split()[3], (pad, pad))

    k = (2 * SCALE) * 2 + 1
    contour = Image.new("RGBA", (cw, ch), (255, 255, 255, 255))
    contour.putalpha(mask_pad.filter(ImageFilter.MaxFilter(k)))
    canvas.alpha_composite(contour)

    mono_pad = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    mono_pad.paste(mono, (pad, pad), mono)
    canvas.alpha_composite(mono_pad)
    return canvas


def to_1x_rgb(scr: Image.Image) -> Image.Image:
    return scr.convert("RGB").resize((W, H), Image.Resampling.LANCZOS)


# ---------------------------------------------------------------------------
# Splash screens
# ---------------------------------------------------------------------------

def render_splash_cyber() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (5, 6, 12, 255))
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    cx = CW // 2
    draw_centered_text(d, cx, 7 * SCALE, "SYS.BOOT // NODE READY", get_font(SPACE_MONO, 7.6 * SCALE), (122, 130, 166, 255))

    rac = make_raccoon_cyberpunk(72 * SCALE)
    scr.alpha_composite(rac, (cx - rac.width // 2, 54 * SCALE - rac.height // 2))

    d = ImageDraw.Draw(scr, "RGBA")
    f_title = get_font(ORBITRON, 19 * SCALE)
    b1 = d.textbbox((0, 0), "//", font=f_title)
    b2 = d.textbbox((0, 0), "MUJINA", font=f_title)
    w1, w2 = b1[2] - b1[0], b2[2] - b2[0]
    gap = 6 * SCALE
    tx = cx - (w1 + gap + w2) // 2
    ty = 90 * SCALE
    tg = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    tgd = ImageDraw.Draw(tg, "RGBA")
    tgd.text((tx, ty), "//", font=f_title, fill=(255, 46, 151, 210))
    tgd.text((tx + w1 + gap, ty), "MUJINA", font=f_title, fill=(0, 240, 255, 200))
    scr.alpha_composite(tg.filter(ImageFilter.GaussianBlur(radius=2.6 * SCALE)))

    d = ImageDraw.Draw(scr, "RGBA")
    d.text((tx, ty), "//", font=f_title, fill=(255, 46, 151, 255))
    d.text((tx + w1 + gap, ty), "MUJINA", font=f_title, fill=(255, 255, 255, 255))

    pw, ph = 132 * SCALE, 17 * SCALE
    px, py = cx - pw // 2, 120 * SCALE
    draw_chamfered_card(d, px, py, pw, ph, 4 * SCALE, (10, 20, 34, 225), (0, 240, 255, 165), 1.1 * SCALE)
    d.ellipse([px + 8 * SCALE, py + 6 * SCALE, px + 13 * SCALE, py + 11 * SCALE], fill=(27, 255, 156, 255))
    d.text((px + 18 * SCALE, py + 3.5 * SCALE), "NERDQX // T-DISPLAY", font=get_font(SPACE_MONO, 7.6 * SCALE), fill=(224, 230, 255, 255))
    return scr


def render_splash_minimal() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (216, 232, 252, 255))
    draw_minimal_bg(scr)
    cx = CW // 2
    mcy = 48 * SCALE
    mr = 38 * SCALE
    med = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    md = ImageDraw.Draw(med, "RGBA")
    md.ellipse([cx - mr, mcy - mr + 3 * SCALE, cx + mr, mcy + mr + 3 * SCALE], fill=(115, 135, 170, 55))
    md.ellipse([cx - mr, mcy - mr, cx + mr, mcy + mr], fill=(255, 255, 255, 195), outline=(255, 255, 255, 250), width=int(1.6 * SCALE))
    scr.alpha_composite(med)

    rac = resize_raccoon(62 * SCALE)
    rx, ry = cx - rac.width // 2, mcy - rac.height // 2
    scr.alpha_composite(rac, (rx, ry))

    d = ImageDraw.Draw(scr, "RGBA")
    draw_centered_text(d, cx, 90 * SCALE, "MUJINA", get_font(SEGOE_BOLD, 21 * SCALE), (30, 41, 59, 255))
    d.rounded_rectangle([cx - 17 * SCALE, 117 * SCALE, cx + 17 * SCALE, 120 * SCALE], radius=1.5 * SCALE, fill=(0, 102, 204, 255))

    pw, ph = 128 * SCALE, 18 * SCALE
    px, py = cx - pw // 2, 126 * SCALE
    draw_glass_card(scr, px, py, pw, ph, 9 * SCALE, (255, 255, 255, 195), (255, 255, 255, 250), (115, 135, 170, 55), rim_w=1.2)
    d = ImageDraw.Draw(scr, "RGBA")
    d.ellipse([px + 9 * SCALE, py + 6 * SCALE, px + 14 * SCALE, py + 11 * SCALE], fill=(16, 185, 129, 255))
    d.text((px + 19 * SCALE, py + 3.2 * SCALE), "NerdQX • T-Display", font=get_font(SEGOE_BOLD, 8.2 * SCALE), fill=(92, 111, 132, 255))
    return scr


def render_splash_parasite() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (0, 0, 0, 255))
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW - 1, CH - 1], outline=(212, 212, 212, 255), width=SCALE)
    cx = CW // 2
    draw_centered_text(d, cx, 7 * SCALE, "[ SYS // INIT ]", get_font(COURIER_BOLD, 7.8 * SCALE), (155, 155, 155, 255))

    rac = make_raccoon_parasite(64 * SCALE)
    scr.alpha_composite(rac, (cx - rac.width // 2, 52 * SCALE - rac.height // 2))

    d = ImageDraw.Draw(scr, "RGBA")
    bw, bh = 148 * SCALE, 26 * SCALE
    bx, by = cx - bw // 2, 88 * SCALE
    d.rectangle([bx, by, bx + bw, by + bh], fill=(0, 0, 0, 255), outline=(212, 212, 212, 255), width=SCALE)
    draw_centered_text(d, cx, by + 4 * SCALE, "// MUJINA //", get_font(COURIER_BOLD, 16 * SCALE), (255, 255, 255, 255))

    sw, sh = 128 * SCALE, 15 * SCALE
    sx, sy = cx - sw // 2, 119 * SCALE
    d.rectangle([sx, sy, sx + sw, sy + sh], fill=(255, 255, 255, 255))
    draw_centered_text(d, cx, sy + 2.5 * SCALE, "NERDQX : ONLINE", get_font(COURIER_BOLD, 8.2 * SCALE), (0, 0, 0, 255))
    return scr


# ---------------------------------------------------------------------------
# Main HUD templates (Header + CutoffDial tracks + 45-deg traces + 4 Cards)
# ---------------------------------------------------------------------------

HUD_CARDS = [
    (186, 26, 128, 29, "CHIP"),
    (186, 59, 128, 29, "VR"),
    (186, 92, 128, 29, "EFF"),
    (186, 125, 128, 29, "FAN"),
]
DIAL_CX, DIAL_CY = 58, 134
TRACE_STARTS = [(DIAL_CX + 76, DIAL_CY - 65), (DIAL_CX + 92, DIAL_CY - 40), (DIAL_CX + 99, DIAL_CY - 12), (DIAL_CX + 99, DIAL_CY + 16)]


def render_hud_cyber() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (5, 6, 12, 255))
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")

    # Header bar
    d.rectangle([0, 0, CW, 22 * SCALE], fill=(10, 15, 28, 235))
    d.line([(0, 22 * SCALE), (CW, 22 * SCALE)], fill=(0, 240, 255, 150), width=SCALE)
    f_b = get_font(ORBITRON, 12.5 * SCALE)
    d.text((6 * SCALE, 3.5 * SCALE), "//", font=f_b, fill=(255, 46, 151, 255))
    d.text((25 * SCALE, 3.5 * SCALE), "MUJINA", font=f_b, fill=(255, 255, 255, 255))

    # CutoffDial tracks
    dcx, dcy = DIAL_CX * SCALE, DIAL_CY * SCALE
    draw_smooth_arc(d, dcx, dcy, 96 * SCALE, 0, 360, 8 * SCALE, (18, 28, 44, 235), round_caps=False)
    draw_smooth_arc(d, dcx, dcy, 84 * SCALE, 0, 360, 4 * SCALE, (0, 240, 255, 36), round_caps=False)
    draw_smooth_arc(d, dcx, dcy, 73 * SCALE, 0, 360, 2 * SCALE, (255, 176, 0, 32), round_caps=False)

    f_lbl = get_font(SPACE_MONO, 7.6 * SCALE)
    accents = [(255, 176, 0, 255), (0, 240, 255, 255), (27, 255, 156, 255), (255, 46, 151, 255)]
    for i, (cx, cy, cw, ch, lbl) in enumerate(HUD_CARDS):
        sx, sy = TRACE_STARTS[i][0] * SCALE, TRACE_STARTS[i][1] * SCALE
        ex, ey = cx * SCALE, (cy + ch // 2) * SCALE
        kx = min(sx + abs(ey - sy), ex - 8 * SCALE)
        d.line([(sx, sy), (kx, ey), (ex, ey)], fill=(0, 240, 255, 145), width=int(1.4 * SCALE))
        d.ellipse([sx - 2 * SCALE, sy - 2 * SCALE, sx + 2 * SCALE, sy + 2 * SCALE], fill=(0, 240, 255, 255))
        d.ellipse([ex - 2 * SCALE, ey - 2 * SCALE, ex + 2 * SCALE, ey + 2 * SCALE], fill=accents[i])

        draw_chamfered_card(d, cx * SCALE, cy * SCALE, cw * SCALE, ch * SCALE, 4 * SCALE, (10, 15, 28, 228), (0, 240, 255, 145), 1.1 * SCALE, left_neon=accents[i])
        d.text(((cx + 8) * SCALE, (cy + 9) * SCALE), lbl, font=f_lbl, fill=(118, 152, 182, 255))
    return scr


def render_hud_minimal() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (216, 232, 252, 255))
    draw_minimal_bg(scr)

    glass_fill = (255, 255, 255, 185)
    glass_rim = (255, 255, 255, 248)
    shadow = (115, 135, 170, 48)
    draw_glass_card(scr, 4 * SCALE, 2 * SCALE, 312 * SCALE, 18 * SCALE, 9 * SCALE, (255, 255, 255, 208), glass_rim, shadow, rim_w=1.1)

    dcx, dcy = DIAL_CX * SCALE, DIAL_CY * SCALE
    rim_l = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(rim_l), dcx, dcy, 96 * SCALE, 0, 360, 9.5 * SCALE, (255, 255, 255, 205))
    draw_smooth_arc(ImageDraw.Draw(rim_l), dcx, dcy, 84 * SCALE, 0, 360, 5.5 * SCALE, (255, 255, 255, 185))
    scr.alpha_composite(rim_l)

    tr_l = Image.new("RGBA", (CW, CH), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(tr_l), dcx, dcy, 96 * SCALE, 0, 360, 7.5 * SCALE, (15, 23, 42, 24))
    draw_smooth_arc(ImageDraw.Draw(tr_l), dcx, dcy, 84 * SCALE, 0, 360, 4.0 * SCALE, (15, 23, 42, 20))
    scr.alpha_composite(tr_l)

    d = ImageDraw.Draw(scr, "RGBA")
    for i, (cx, cy, cw, ch, _) in enumerate(HUD_CARDS):
        sx, sy = TRACE_STARTS[i][0] * SCALE, TRACE_STARTS[i][1] * SCALE
        ex, ey = cx * SCALE, (cy + ch // 2) * SCALE
        kx = min(sx + abs(ey - sy), ex - 8 * SCALE)
        d.line([(sx, sy), (kx, ey), (ex, ey)], fill=(100, 125, 155, 140), width=int(1.4 * SCALE))
        d.ellipse([sx - 2 * SCALE, sy - 2 * SCALE, sx + 2 * SCALE, sy + 2 * SCALE], fill=(0, 102, 204, 200))

    for cx, cy, cw, ch, _ in HUD_CARDS:
        draw_glass_card(scr, cx * SCALE, cy * SCALE, cw * SCALE, ch * SCALE, 8 * SCALE, glass_fill, glass_rim, shadow, rim_w=1.2)

    d = ImageDraw.Draw(scr, "RGBA")
    d.text((12 * SCALE, 3 * SCALE), "MUJINA", font=get_font(SEGOE_BOLD, 12.5 * SCALE), fill=(30, 41, 59, 255))
    f_lbl = get_font(SEGOE_BOLD, 8.0 * SCALE)
    for cx, cy, cw, ch, lbl in HUD_CARDS:
        d.text(((cx + 8) * SCALE, (cy + 9) * SCALE), lbl.title() if lbl != "VR" and lbl != "EFF" else lbl, font=f_lbl, fill=(92, 111, 132, 255))
    return scr


def render_hud_parasite() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (0, 0, 0, 255))
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW - 1, CH - 1], outline=(255, 255, 255, 255), width=SCALE)
    d.line([(0, 22 * SCALE), (CW, 22 * SCALE)], fill=(255, 255, 255, 255), width=SCALE)
    d.text((6 * SCALE, 4 * SCALE), "MUJINA // QX", font=get_font(COURIER_BOLD, 12 * SCALE), fill=(255, 255, 255, 255))

    dcx, dcy = DIAL_CX * SCALE, DIAL_CY * SCALE
    for i in range(36):
        ang = math.radians(i * 10.0)
        major = (i % 3 == 0)
        r_out = 100 * SCALE
        r_in = (91 if major else 94) * SCALE
        d.line([(dcx + r_in * math.cos(ang), dcy + r_in * math.sin(ang)), (dcx + r_out * math.cos(ang), dcy + r_out * math.sin(ang))],
               fill=(255, 255, 255, 255) if major else (68, 68, 68, 255), width=int((2.0 if major else 1.0) * SCALE))
    draw_smooth_arc(d, dcx, dcy, 84 * SCALE, 0, 360, 2 * SCALE, (70, 70, 70, 255), round_caps=False)

    f_lbl = get_font(COURIER_BOLD, 8.0 * SCALE)
    for i, (cx, cy, cw, ch, lbl) in enumerate(HUD_CARDS):
        sx, sy = TRACE_STARTS[i][0] * SCALE, TRACE_STARTS[i][1] * SCALE
        ex, ey = cx * SCALE, (cy + ch // 2) * SCALE
        kx = min(sx + abs(ey - sy), ex - 8 * SCALE)
        d.line([(sx, sy), (kx, ey), (ex, ey)], fill=(170, 170, 170, 255), width=SCALE)
        d.rectangle([cx * SCALE, cy * SCALE, (cx + cw) * SCALE, (cy + ch) * SCALE], fill=(0, 0, 0, 255), outline=(212, 212, 212, 255), width=SCALE)
        d.rectangle([cx * SCALE, cy * SCALE, (cx + 3) * SCALE, (cy + ch) * SCALE], fill=(255, 51, 51, 255) if i == 0 else (255, 255, 255, 255))
        d.text(((cx + 8) * SCALE, (cy + 9) * SCALE), lbl, font=f_lbl, fill=(165, 165, 165, 255))
    return scr


# ---------------------------------------------------------------------------
# Front Wi-Fi AP & QR Screen templates (left QR box + right info card)
# ---------------------------------------------------------------------------

def render_qr_cyber() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (5, 6, 12, 255))
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW, 22 * SCALE], fill=(10, 15, 28, 235))
    d.line([(0, 22 * SCALE), (CW, 22 * SCALE)], fill=(0, 240, 255, 150), width=SCALE)
    f_b = get_font(ORBITRON, 12.5 * SCALE)
    d.text((6 * SCALE, 3.5 * SCALE), "//", font=f_b, fill=(255, 46, 151, 255))
    d.text((25 * SCALE, 3.5 * SCALE), "MUJINA", font=f_b, fill=(255, 255, 255, 255))

    draw_chamfered_card(d, 8 * SCALE, 27 * SCALE, 134 * SCALE, 134 * SCALE, 5 * SCALE, (10, 15, 28, 228), (0, 240, 255, 185), 1.2 * SCALE)
    d.rectangle([13 * SCALE, 32 * SCALE, 137 * SCALE, 156 * SCALE], fill=(255, 255, 255, 255))
    draw_chamfered_card(d, 148 * SCALE, 27 * SCALE, 164 * SCALE, 134 * SCALE, 5 * SCALE, (10, 15, 28, 228), (255, 176, 0, 150), 1.1 * SCALE, left_neon=(0, 240, 255, 255))
    return scr


def render_qr_minimal() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (216, 232, 252, 255))
    draw_minimal_bg(scr)
    glass_fill = (255, 255, 255, 195)
    glass_rim = (255, 255, 255, 248)
    shadow = (115, 135, 170, 48)
    draw_glass_card(scr, 4 * SCALE, 2 * SCALE, 312 * SCALE, 18 * SCALE, 9 * SCALE, (255, 255, 255, 212), glass_rim, shadow, rim_w=1.1)
    draw_glass_card(scr, 8 * SCALE, 27 * SCALE, 134 * SCALE, 134 * SCALE, 10 * SCALE, glass_fill, glass_rim, shadow, rim_w=1.3)
    draw_glass_card(scr, 148 * SCALE, 27 * SCALE, 164 * SCALE, 134 * SCALE, 10 * SCALE, glass_fill, glass_rim, shadow, rim_w=1.3)
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([13 * SCALE, 32 * SCALE, 137 * SCALE, 156 * SCALE], fill=(255, 255, 255, 255))
    d.text((12 * SCALE, 3 * SCALE), "MUJINA", font=get_font(SEGOE_BOLD, 12.5 * SCALE), fill=(30, 41, 59, 255))
    return scr


def render_qr_parasite() -> Image.Image:
    scr = Image.new("RGBA", (CW, CH), (0, 0, 0, 255))
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, CW - 1, CH - 1], outline=(255, 255, 255, 255), width=SCALE)
    d.line([(0, 22 * SCALE), (CW, 22 * SCALE)], fill=(255, 255, 255, 255), width=SCALE)
    d.text((6 * SCALE, 4 * SCALE), "MUJINA // QX", font=get_font(COURIER_BOLD, 12 * SCALE), fill=(255, 255, 255, 255))
    d.rectangle([8 * SCALE, 27 * SCALE, 142 * SCALE, 161 * SCALE], fill=(255, 255, 255, 255), outline=(255, 255, 255, 255), width=SCALE)
    d.rectangle([148 * SCALE, 27 * SCALE, 312 * SCALE, 161 * SCALE], fill=(0, 0, 0, 255), outline=(212, 212, 212, 255), width=SCALE)
    d.rectangle([148 * SCALE, 27 * SCALE, 151 * SCALE, 161 * SCALE], fill=(255, 51, 51, 255))
    return scr


# ---------------------------------------------------------------------------
# Rust codegen
# ---------------------------------------------------------------------------

def rgb_to_be_rgb565_array(img_1x: Image.Image) -> np.ndarray:
    arr = np.asarray(img_1x, dtype=np.uint16)
    r = (arr[:, :, 0] & 0xF8) << 8
    g = (arr[:, :, 1] & 0xFC) << 3
    b = arr[:, :, 2] >> 3
    native = r | g | b
    return ((native & 0xFF) << 8) | (native >> 8)


def generate_ui_assets_rs(out_path: Path, preview_dir: Path | None):
    assets = [
        ("BG_SPLASH_CYBER", render_splash_cyber),
        ("BG_SPLASH_MINIMAL", render_splash_minimal),
        ("BG_SPLASH_PARASITE", render_splash_parasite),
        ("BG_HUD_CYBER", render_hud_cyber),
        ("BG_HUD_MINIMAL", render_hud_minimal),
        ("BG_HUD_PARASITE", render_hud_parasite),
        ("BG_QR_CYBER", render_qr_cyber),
        ("BG_QR_MINIMAL", render_qr_minimal),
        ("BG_QR_PARASITE", render_qr_parasite),
    ]

    lines = [
        "// Auto-generated 4x-supersampled 320x170 big-endian RGB565 templates for LilyGO T-Display-S3.",
        "#![allow(clippy::all)]",
        "",
    ]

    for sym, fn in assets:
        scr = fn()
        img_1x = to_1x_rgb(scr)
        if preview_dir is not None:
            preview_dir.mkdir(parents=True, exist_ok=True)
            img_1x.save(preview_dir / f"lilygo_{sym.lower()}.png")
        flat = rgb_to_be_rgb565_array(img_1x).reshape(-1)
        lines.append(f"pub static {sym}: [u16; {len(flat)}] = [")
        for i in range(0, len(flat), 16):
            lines.append("    " + ",".join(f"0x{int(v):04X}" for v in flat[i:i + 16]) + ",")
        lines.append("];")
        lines.append("")

    out_path.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print(f"Wrote {out_path} ({out_path.stat().st_size:,} bytes)")


FONT_CHARS = [chr(c) for c in range(32, 127)] + ["\u00b0", "\u2022"]
FONT_SPECS = [
    ("FONT_ORBITRON_22", ORBITRON, 22.0),
    ("FONT_ORBITRON_11", ORBITRON, 11.0),
    ("FONT_SPACE_8", SPACE_MONO, 8.2),
    ("FONT_BAHN_24", BAHNSCHRIFT, 24.0),
    ("FONT_BAHN_12", BAHNSCHRIFT, 12.0),
    ("FONT_SEGOE_8", SEGOE_BOLD, 8.4),
    ("FONT_COURIER_22", COURIER_BOLD, 22.0),
    ("FONT_COURIER_11", COURIER_BOLD, 11.0),
    ("FONT_COURIER_8", COURIER_BOLD, 8.4),
    ("FONT_ORBITRON_15", ORBITRON, 15.0),
    ("FONT_BAHN_16", BAHNSCHRIFT, 16.0),
    ("FONT_COURIER_15", COURIER_BOLD, 15.0),
]


def generate_ui_fonts_rs(out_path: Path):
    SS = 4
    lines = [
        "// Auto-generated 4x-supersampled 8-bit alpha glyph fonts for 320x170 RGB565.",
        "#![allow(clippy::all)]",
        "",
        "#[derive(Clone, Copy)]",
        "pub struct UiGlyph {",
        "    pub codepoint: u16,",
        "    pub width: u8,",
        "    pub height: u8,",
        "    pub x_off: i8,",
        "    pub y_off: i8,",
        "    pub advance: u8,",
        "    pub data_offset: u32,",
        "}",
        "",
        "pub struct UiFont {",
        "    pub line_height: u8,",
        "    pub glyphs: &'static [UiGlyph],",
        "    pub bitmap: &'static [u8],",
        "}",
        "",
    ]

    for sym, ttf_path, pt_size in FONT_SPECS:
        font_ss = get_font(ttf_path, pt_size * SS)
        glyphs = []
        bitmap_bytes = bytearray()

        for ch in FONT_CHARS:
            canvas = Image.new("L", (240, 240), 0)
            cd = ImageDraw.Draw(canvas)
            ox, oy = 32, 32
            cd.text((ox, oy), ch, font=font_ss, fill=255)
            adv = max(1, int(round(cd.textlength(ch, font=font_ss) / SS)))

            cropped_1x = canvas.resize((240 // SS, 240 // SS), Image.Resampling.LANCZOS)
            arr = np.array(cropped_1x, dtype=np.uint8)
            nz = np.argwhere(arr > 4)
            if len(nz) == 0:
                glyphs.append((ord(ch), 0, 0, 0, 0, adv, len(bitmap_bytes)))
            else:
                y0, x0 = nz.min(axis=0)
                y1, x1 = nz.max(axis=0)
                sub = arr[y0:y1 + 1, x0:x1 + 1]
                sh, sw = sub.shape
                x_off = int(x0 - (ox // SS))
                y_off = int(y0 - (oy // SS))
                data_off = len(bitmap_bytes)
                bitmap_bytes.extend(sub.tobytes())
                glyphs.append((ord(ch), sw, sh, x_off, y_off, adv, data_off))

        lines.append(f"static {sym}_BITMAP: &[u8] = &[")
        if not bitmap_bytes:
            lines.append("    0,")
        else:
            for i in range(0, len(bitmap_bytes), 24):
                lines.append("    " + ",".join(str(b) for b in bitmap_bytes[i:i + 24]) + ",")
        lines.append("];")
        lines.append(f"static {sym}_GLYPHS: &[UiGlyph] = &[")
        for g in glyphs:
            lines.append(f"    UiGlyph {{ codepoint: {g[0]}, width: {g[1]}, height: {g[2]}, x_off: {g[3]}, y_off: {g[4]}, advance: {g[5]}, data_offset: {g[6]} }},")
        lines.append("];")
        lines.append(f"pub static {sym}: UiFont = UiFont {{ line_height: {int(round(pt_size * 1.2))}, glyphs: {sym}_GLYPHS, bitmap: {sym}_BITMAP }};")
        lines.append("")

    out_path.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print(f"Wrote {out_path} ({out_path.stat().st_size:,} bytes)")


def main():
    parser = argparse.ArgumentParser(description="Generate Nano 3S-styled 320x170 Rust assets and fonts.")
    parser.add_argument("--assets-out", type=Path, default=DEFAULT_ASSETS_RS)
    parser.add_argument("--fonts-out", type=Path, default=DEFAULT_FONTS_RS)
    parser.add_argument("--save-previews", type=Path, default=None)
    args = parser.parse_args()
    generate_ui_assets_rs(args.assets_out, args.save_previews)
    generate_ui_fonts_rs(args.fonts_out)


if __name__ == "__main__":
    main()
