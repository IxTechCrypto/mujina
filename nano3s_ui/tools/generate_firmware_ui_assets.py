"""Standalone generator for Nano 3S LCD 240x240 RGB565 background arrays and bitmap fonts.

Requirements: numpy, pymupdf, Pillow
"""

import argparse
import math
import os
from pathlib import Path

import numpy as np
import pymupdf
from PIL import Image, ImageDraw, ImageFilter, ImageFont

TOOLS_DIR = Path(__file__).resolve().parent
NANO3S_UI_DIR = TOOLS_DIR.parent
REPO_ROOT = NANO3S_UI_DIR.parent
SVG_PATH = str(REPO_ROOT / "mujina-miner" / "assets" / "mujina-head-mark.svg")
PARASITE_PNG_PATH = str(TOOLS_DIR / "assets" / "parasite_logo.png")
DEFAULT_ASSETS_H = NANO3S_UI_DIR / "c_driver" / "ui_assets.h"
DEFAULT_FONTS_H = NANO3S_UI_DIR / "c_driver" / "ui_fonts.h"


def _find_font(candidates):
    for cand in candidates:
        if os.path.exists(cand):
            return cand
    return candidates[0]


FONT_DIRS = [
    str(TOOLS_DIR / "fonts"),
    r"D:\github\nexus-miner-app\assets\fonts",
    "/mnt/d/github/nexus-miner-app/assets/fonts",
    r"C:\Windows\Fonts",
    "/mnt/c/Windows/Fonts",
    "/usr/share/fonts/truetype",
]

ORBITRON = _find_font([os.path.join(d, "Orbitron-Bold.ttf") for d in FONT_DIRS])
SPACE_MONO = _find_font([os.path.join(d, "SpaceMono-Regular.ttf") for d in FONT_DIRS])
BAHNSCHRIFT = _find_font([os.path.join(d, "bahnschrift.ttf") for d in FONT_DIRS])
SEGOE_BOLD = _find_font([os.path.join(d, "segoeuib.ttf") for d in FONT_DIRS])
SEGOE_REG = _find_font([os.path.join(d, "segoeui.ttf") for d in FONT_DIRS])
COURIER = _find_font([os.path.join(d, "cour.ttf") for d in FONT_DIRS])
COURIER_BOLD = _find_font([os.path.join(d, "courbd.ttf") for d in FONT_DIRS])

SCALE = 4
TOTAL_SIZE = 280 * SCALE
CX = TOTAL_SIZE // 2
CY = TOTAL_SIZE // 2
SCR_R = 118 * SCALE


def get_font(path, size):
    if not os.path.exists(path):
        raise FileNotFoundError(f"Missing required font: {path}")
    return ImageFont.truetype(path, int(size))


def make_circle_mask(radius):
    # Supersampled 2x mask for crisp anti-aliased circular glass boundary
    hi = TOTAL_SIZE * 2
    mask_hi = Image.new("L", (hi, hi), 0)
    md = ImageDraw.Draw(mask_hi)
    c = hi // 2
    r = int(radius * 2)
    md.ellipse([c - r, c - r, c + r, c + r], fill=255)
    return mask_hi.resize((TOTAL_SIZE, TOTAL_SIZE), Image.Resampling.LANCZOS)


CIRCLE_MASK = make_circle_mask(SCR_R)


def draw_hardware_bezel(img):
    draw = ImageDraw.Draw(img)
    draw.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(10, 12, 16, 255))
    bezel_r = 136 * SCALE
    for r in range(bezel_r, int(119 * SCALE), -1):
        t = (bezel_r - r) / (17.0 * SCALE)
        shade = int(18 + 38 * (t ** 1.15))
        draw.ellipse(
            [CX - r, CY - r, CX + r, CY + r],
            outline=(shade, shade + 2, shade + 5, 255),
            width=3,
        )
    draw.ellipse(
        [CX - 120 * SCALE, CY - 120 * SCALE, CX + 120 * SCALE, CY + 120 * SCALE],
        outline=(58, 64, 76, 255),
        width=int(2.2 * SCALE),
    )
    draw.ellipse(
        [CX - 118.5 * SCALE, CY - 118.5 * SCALE, CX + 118.5 * SCALE, CY + 118.5 * SCALE],
        outline=(16, 18, 24, 255),
        width=int(1.5 * SCALE),
    )


def composite_screen(base_img, screen_layer):
    """Composites screen_layer strictly inside the circular LCD mask while keeping base opaque."""
    clipped = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    clipped.paste(screen_layer, (0, 0), CIRCLE_MASK)
    base_img.alpha_composite(clipped)

    # Subtle glass edge inner vignette drawn on an RGBA overlay so base alpha stays 255
    vig = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    vd = ImageDraw.Draw(vig, "RGBA")
    for i in range(int(3.5 * SCALE)):
        alpha = int(60 - i * 4.2)
        if alpha > 0:
            vd.ellipse(
                [CX - SCR_R + i, CY - SCR_R + i, CX + SCR_R - i, CY + SCR_R - i],
                outline=(0, 0, 0, alpha),
                width=2,
            )
    vig_clipped = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    vig_clipped.paste(vig, (0, 0), CIRCLE_MASK)
    base_img.alpha_composite(vig_clipped)


def lerp_color(c1, c2, t):
    t = max(0.0, min(1.0, t))
    return tuple(int(c1[i] + (c2[i] - c1[i]) * t) for i in range(len(c1)))


def draw_wedge_arc(draw, cx, cy, r_mid, start_deg, end_deg, width, color):
    """Draws a razor-sharp flat-capped annulus sector polygon with zero gap bleed."""
    sweep = end_deg - start_deg
    if abs(sweep) < 0.01:
        return
    steps = max(4, int(abs(sweep) * 1.5))
    r_out = r_mid + width / 2.0
    r_in = r_mid - width / 2.0
    outer_pts = []
    inner_pts = []
    for i in range(steps + 1):
        ang = math.radians(start_deg + sweep * (i / steps))
        ca, sa = math.cos(ang), math.sin(ang)
        outer_pts.append((cx + r_out * ca, cy + r_out * sa))
        inner_pts.append((cx + r_in * ca, cy + r_in * sa))
    poly = outer_pts + list(reversed(inner_pts))
    draw.polygon(poly, fill=color)


def draw_smooth_arc(draw, cx, cy, r_mid, start_deg, end_deg, width, color, color_end=None, round_caps=True):
    """Draws either flat-capped wedge arcs or silky rounded-cap arcs."""
    if not round_caps and color_end is None:
        draw_wedge_arc(draw, cx, cy, r_mid, start_deg, end_deg, width, color)
        return
    sweep = end_deg - start_deg
    if abs(sweep) < 0.01:
        return
    if not round_caps and color_end is not None:
        # Multi-slice gradient wedges
        slices = max(12, int(abs(sweep) / 3))
        for s in range(slices):
            t0 = s / slices
            t1 = (s + 1) / slices
            a0 = start_deg + sweep * t0
            a1 = start_deg + sweep * t1
            col = lerp_color(color, color_end, (t0 + t1) * 0.5)
            draw_wedge_arc(draw, cx, cy, r_mid, a0, a1 + 0.15, width, col)
        return

    steps = max(24, int(abs(sweep) * 3))
    rad = width / 2.0
    for i in range(steps + 1):
        t = i / steps
        ang = math.radians(start_deg + sweep * t)
        px = cx + r_mid * math.cos(ang)
        py = cy + r_mid * math.sin(ang)
        col = lerp_color(color, color_end, t) if color_end else color
        draw.ellipse([px - rad, py - rad, px + rad, py + rad], fill=col)


def draw_chamfered_card(draw, x, y, w, h, chamfer, fill, outline, outline_w, left_neon_color=None):
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
        p1 = pts[i]
        p2 = pts[(i + 1) % len(pts)]
        draw.line([p1, p2], fill=outline, width=int(outline_w))
    if left_neon_color is not None:
        bar_w = int(2.6 * SCALE)
        draw.line([(x, y), (x, y + h - chamfer)], fill=left_neon_color, width=bar_w)


def draw_glass_card(scr, x, y, w, h, radius, fill, rim, shadow_col, rim_w=1.3):
    """Draws a true alpha-blended frosted glass card with drop shadow and specular rim."""
    layer = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    ld = ImageDraw.Draw(layer, "RGBA")
    # Soft shadow
    ld.rounded_rectangle(
        [x, y + 2.5 * SCALE, x + w, y + h + 2.5 * SCALE],
        radius=radius,
        fill=shadow_col,
    )
    # Frosted body
    ld.rounded_rectangle(
        [x, y, x + w, y + h],
        radius=radius,
        fill=fill,
        outline=rim,
        width=int(rim_w * SCALE),
    )
    scr.alpha_composite(layer)


def draw_cyberpunk_bg(scr):
    """Draws #05060c background, subtle cyan grid, radial glow, and CRT scanlines."""
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(5, 6, 12, 255))
    grid_step = 18 * SCALE
    grid_col = (0, 240, 255, 18)
    for gx in range(CX % grid_step, TOTAL_SIZE, grid_step):
        d.line([(gx, 0), (gx, TOTAL_SIZE)], fill=grid_col, width=max(1, SCALE // 2))
    for gy in range(CY % grid_step, TOTAL_SIZE, grid_step):
        d.line([(0, gy), (TOTAL_SIZE, gy)], fill=grid_col, width=max(1, SCALE // 2))

    glow = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow, "RGBA")
    gd.ellipse([CX - 75 * SCALE, CY - 75 * SCALE, CX + 75 * SCALE, CY + 75 * SCALE], fill=(0, 240, 255, 22))
    glow = glow.filter(ImageFilter.GaussianBlur(radius=24 * SCALE))
    scr.alpha_composite(glow)

    sd = ImageDraw.Draw(scr, "RGBA")
    for y in range(CY - SCR_R, CY + SCR_R, int(3 * SCALE)):
        dy = y - CY
        dx = int(math.sqrt(max(0, SCR_R * SCR_R - dy * dy)))
        sd.line([(CX - dx, y), (CX + dx, y)], fill=(0, 0, 0, 48), width=max(1, SCALE))


def draw_minimal_bg(scr, dark=False):
    """Draws smooth pastel (light) or midnight slate (dark) liquid gradient inside circle."""
    d = ImageDraw.Draw(scr, "RGBA")
    if not dark:
        d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(216, 232, 252, 255))
        blobs = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
        bd = ImageDraw.Draw(blobs, "RGBA")
        bd.ellipse([CX - 145 * SCALE, CY - 145 * SCALE, CX + 35 * SCALE, CY + 35 * SCALE], fill=(168, 212, 255, 225))
        bd.ellipse([CX - 20 * SCALE, CY - 95 * SCALE, CX + 140 * SCALE, CY + 80 * SCALE], fill=(218, 198, 250, 220))
        bd.ellipse([CX - 95 * SCALE, CY + 5 * SCALE, CX + 125 * SCALE, CY + 145 * SCALE], fill=(250, 218, 232, 225))
        blobs = blobs.filter(ImageFilter.GaussianBlur(radius=28 * SCALE))
        scr.alpha_composite(blobs)
    else:
        d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(12, 18, 34, 255))
        blobs = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
        bd = ImageDraw.Draw(blobs, "RGBA")
        bd.ellipse([CX - 135 * SCALE, CY - 135 * SCALE, CX + 35 * SCALE, CY + 35 * SCALE], fill=(29, 78, 216, 95))
        bd.ellipse([CX - 15 * SCALE, CY - 60 * SCALE, CX + 130 * SCALE, CY + 95 * SCALE], fill=(99, 102, 241, 80))
        bd.ellipse([CX - 95 * SCALE, CY + 20 * SCALE, CX + 95 * SCALE, CY + 140 * SCALE], fill=(14, 165, 233, 65))
        blobs = blobs.filter(ImageFilter.GaussianBlur(radius=30 * SCALE))
        scr.alpha_composite(blobs)


def draw_centered_text(draw, cx, y, text, font, fill):
    bbox = draw.textbbox((0, 0), text, font=font)
    w = bbox[2] - bbox[0]
    draw.text((cx - w // 2, y), text, font=font, fill=fill)
    return w, bbox[3] - bbox[1]


_RACCOON_CACHE = None


def load_mujina_raccoon():
    """Rasterizes canonical mujina-head-mark.svg with full alpha transparency and crops tightly."""
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


def resize_raccoon(target_width):
    src = load_mujina_raccoon()
    aspect = src.height / float(src.width)
    th = max(1, int(round(target_width * aspect)))
    return src.resize((int(target_width), th), Image.Resampling.LANCZOS)


def make_raccoon_cyberpunk(target_width):
    """Canonical Mujina raccoon with multi-radius #00f0ff cyan glow and faint HUD scanlines."""
    sprite = resize_raccoon(target_width)
    pad = int(18 * SCALE)
    cw, ch = sprite.width + 2 * pad, sprite.height + 2 * pad
    canvas = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))

    alpha = sprite.split()[3]
    mask_pad = Image.new("L", (cw, ch), 0)
    mask_pad.paste(alpha, (pad, pad))

    outer_glow = Image.new("RGBA", (cw, ch), (0, 240, 255, 0))
    outer_glow.putalpha(mask_pad)
    outer_glow = Image.composite(
        Image.new("RGBA", (cw, ch), (0, 240, 255, 225)),
        Image.new("RGBA", (cw, ch), (0, 0, 0, 0)),
        mask_pad,
    ).filter(ImageFilter.GaussianBlur(radius=6.5 * SCALE))

    inner_glow = Image.composite(
        Image.new("RGBA", (cw, ch), (0, 240, 255, 240)),
        Image.new("RGBA", (cw, ch), (0, 0, 0, 0)),
        mask_pad,
    ).filter(ImageFilter.GaussianBlur(radius=2.2 * SCALE))

    canvas.alpha_composite(outer_glow)
    canvas.alpha_composite(inner_glow)

    sprite_pad = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    sprite_pad.paste(sprite, (pad, pad), sprite)
    canvas.alpha_composite(sprite_pad)

    # Subtle scanline etch matching web HUD header aesthetic
    scan = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    sd = ImageDraw.Draw(scan, "RGBA")
    for y in range(0, ch, int(3 * SCALE)):
        sd.line([(0, y), (cw, y)], fill=(0, 0, 0, 32), width=max(1, SCALE // 2))
    scan.putalpha(Image.composite(scan.split()[3], Image.new("L", (cw, ch), 0), mask_pad))
    canvas.alpha_composite(scan)
    return canvas, pad


def make_raccoon_parasite(target_width):
    """Transforms the canonical raccoon into stark B&W woodcut stencil with crisp white edge and #ff3333 red eye."""
    sprite = resize_raccoon(target_width)
    arr = np.array(sprite, dtype=np.uint8)
    r = arr[:, :, 0].astype(np.float32)
    g = arr[:, :, 1].astype(np.float32)
    b = arr[:, :, 2].astype(np.float32)
    a = arr[:, :, 3]

    lum = 0.299 * r + 0.587 * g + 0.114 * b
    is_vis = a > 55
    is_red = is_vis & (r > 120) & (r > 1.55 * g) & (r > 1.55 * b)

    out = np.zeros_like(arr)
    out[:, :, 3] = a

    # Highlights -> pure white, midtones -> crisp 50% stipple/gray, shadows -> pitch black
    high = is_vis & (~is_red) & (lum >= 112)
    mid = is_vis & (~is_red) & (lum >= 62) & (lum < 112)
    dark = is_vis & (~is_red) & (lum < 62)

    out[high, 0] = 255
    out[high, 1] = 255
    out[high, 2] = 255

    # Clean Bayer-like dot matrix for midtones so fur texture reads retro-digital
    yy, xx = np.indices((arr.shape[0], arr.shape[1]))
    step = max(2, SCALE)
    bayer = (((yy // step) + (xx // step)) % 2) == 0
    out[mid & bayer, 0:3] = 225
    out[mid & (~bayer), 0:3] = 40

    out[dark, 0:3] = 8
    out[is_red, 0] = 255
    out[is_red, 1] = 48
    out[is_red, 2] = 48

    mono = Image.fromarray(out, mode="RGBA")

    # Add crisp 2px white silhouette contour around the raccoon so black mask never disappears on #000000
    pad = int(6 * SCALE)
    cw, ch = mono.width + 2 * pad, mono.height + 2 * pad
    canvas = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    alpha = mono.split()[3]
    mask_pad = Image.new("L", (cw, ch), 0)
    mask_pad.paste(alpha, (pad, pad))

    k = int(2 * SCALE) * 2 + 1
    dilated = mask_pad.filter(ImageFilter.MaxFilter(k))
    contour = Image.new("RGBA", (cw, ch), (255, 255, 255, 255))
    contour.putalpha(dilated)
    canvas.alpha_composite(contour)

    mono_pad = Image.new("RGBA", (cw, ch), (0, 0, 0, 0))
    mono_pad.paste(mono, (pad, pad), mono)
    canvas.alpha_composite(mono_pad)
    return canvas, pad


def make_parasite_mite_sprite(target_width):
    """Converts uploaded dark-on-transparent Parasite PNG into crisp white-on-black OLED stencil."""
    src = Image.open(PARASITE_PNG_PATH).convert("RGBA")
    bbox = src.getbbox()
    if bbox:
        src = src.crop(bbox)
    aspect = src.height / float(src.width)
    th = max(1, int(round(target_width * aspect)))
    sprite = src.resize((int(target_width), th), Image.Resampling.LANCZOS)

    arr = np.array(sprite, dtype=np.uint8)
    r = arr[:, :, 0].astype(np.float32)
    g = arr[:, :, 1].astype(np.float32)
    b = arr[:, :, 2].astype(np.float32)
    a = arr[:, :, 3]
    lum = 0.299 * r + 0.587 * g + 0.114 * b

    out = np.zeros_like(arr)
    vis = a > 40
    # Dark ink body -> pure #FFFFFF; interior bright lines/eye -> pure #000000 cutout
    body = vis & (lum < 150)
    cutout = vis & (lum >= 150)

    out[body, 0:3] = 255
    out[body, 3] = a[body]
    out[cutout, 0:3] = 0
    out[cutout, 3] = a[cutout]
    return Image.fromarray(out, mode="RGBA")


def render_s5_parasite_logo(with_bezel=True):
    """Renders Screen 5 (Parasite Logo Screen after IP on Parasite theme) at 4x."""
    if with_bezel:
        base = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 255))
        draw_hardware_bezel(base)
    scr = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 255))
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(0, 0, 0, 255))

    white = (255, 255, 255, 255)
    dim = (155, 155, 155, 255)
    dark_tick = (68, 68, 68, 255)
    red = (255, 51, 51, 255)
    black = (0, 0, 0, 255)

    # Outer 36 radial ticks + counter-rotating red/white scanner arcs (animated on HW)
    if with_bezel:
        for i in range(36):
            ang = math.radians(i * 10.0)
            major = (i % 3 == 0)
            r_out = 111 * SCALE
            r_in = (103 if major else 106) * SCALE
            d.line(
                [(CX + r_in * math.cos(ang), CY + r_in * math.sin(ang)),
                 (CX + r_out * math.cos(ang), CY + r_out * math.sin(ang))],
                fill=white if major else dark_tick,
                width=int((2.2 if major else 1.2) * SCALE),
            )
        draw_wedge_arc(d, CX, CY, 99 * SCALE, -45, 25, 3.0 * SCALE, red)
        draw_wedge_arc(d, CX, CY, 99 * SCALE, 135, 205, 3.0 * SCALE, white)
    else:
        # Firmware template keeps thin inner ring only; ticks/arcs animate dynamically
        pass

    f_hdr = get_font(COURIER_BOLD, 8.2 * SCALE)
    f_title = get_font(COURIER_BOLD, 17.5 * SCALE)
    f_bar = get_font(COURIER_BOLD, 8.2 * SCALE)

    draw_centered_text(d, CX, CY - 86 * SCALE, "[ PARASITE // NODE ]", f_hdr, dim)

    # Crisp white-on-black Parasite mite centered cleanly inside upper chest
    mite = make_parasite_mite_sprite(86 * SCALE)
    mx = CX - mite.width // 2
    my = CY - 30 * SCALE - mite.height // 2
    scr.alpha_composite(mite, (mx, my))

    d = ImageDraw.Draw(scr, "RGBA")
    border_col = (212, 212, 212, 255)
    bw_1px = max(1, int(round(1.1 * SCALE)))
    bw, bh = 152 * SCALE, 30 * SCALE
    bx, by = CX - bw // 2, CY + 25 * SCALE
    d.rectangle([bx, by, bx + bw, by + bh], fill=black, outline=border_col, width=bw_1px)
    draw_centered_text(d, CX, by + 6 * SCALE, "// PARASITE //", f_title, white)

    sw, sh = 114 * SCALE, 15 * SCALE
    sx, sy = CX - sw // 2, CY + 62 * SCALE
    d.rectangle([sx, sy, sx + sw, sy + sh], fill=white)
    draw_centered_text(d, CX, sy + 2.8 * SCALE, "SWARM : ACTIVE", f_bar, black)

    if with_bezel:
        composite_screen(base, scr)
        return base
    return scr


# ==============================================================================
# 240x240 BASE TEMPLATES (NO DYNAMIC ARCS / NO DYNAMIC VALUES) FOR C FIRMWARE
# ==============================================================================

def make_screen_canvas():
    return Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 255))


def crop_to_240(scr):
    """Crops the 240x240 region out of TOTAL_SIZE (280*SCALE) and downsamples with LANCZOS."""
    margin = (TOTAL_SIZE - 240 * SCALE) // 2
    cropped = scr.crop((margin, margin, margin + 240 * SCALE, margin + 240 * SCALE)).convert("RGB")
    return cropped.resize((240, 240), Image.Resampling.LANCZOS)


def tpl_splash_cyber():
    scr = make_screen_canvas()
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    cyan = (0, 240, 255, 255)
    mag = (255, 46, 151, 255)
    dim = (122, 130, 166, 255)
    white = (255, 255, 255, 255)

    d.ellipse([CX - 98 * SCALE, CY - 98 * SCALE, CX + 98 * SCALE, CY + 98 * SCALE], outline=(0, 240, 255, 42), width=max(1, SCALE))

    f_micro = get_font(SPACE_MONO, 7.6 * SCALE)
    f_title = get_font(ORBITRON, 21.5 * SCALE)
    f_pill = get_font(SPACE_MONO, 7.6 * SCALE)
    draw_centered_text(d, CX, CY - 88 * SCALE, "SYS.BOOT // NODE READY", f_micro, dim)

    rac, _ = make_raccoon_cyberpunk(104 * SCALE)
    scr.alpha_composite(rac, (CX - rac.width // 2, CY - 22 * SCALE - rac.height // 2))

    d = ImageDraw.Draw(scr, "RGBA")
    b1 = d.textbbox((0, 0), "//", font=f_title)
    b2 = d.textbbox((0, 0), "MUJINA", font=f_title)
    w1, w2 = b1[2] - b1[0], b2[2] - b2[0]
    gap_t = 6 * SCALE
    tx = CX - (w1 + gap_t + w2) // 2
    ty = CY + 28 * SCALE
    tglow = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    tgd = ImageDraw.Draw(tglow, "RGBA")
    tgd.text((tx, ty), "//", font=f_title, fill=(255, 46, 151, 210))
    tgd.text((tx + w1 + gap_t, ty), "MUJINA", font=f_title, fill=(0, 240, 255, 200))
    scr.alpha_composite(tglow.filter(ImageFilter.GaussianBlur(radius=2.8 * SCALE)))
    d = ImageDraw.Draw(scr, "RGBA")
    d.text((tx, ty), "//", font=f_title, fill=mag)
    d.text((tx + w1 + gap_t, ty), "MUJINA", font=f_title, fill=white)

    pw, ph = 114 * SCALE, 17 * SCALE
    px, py = CX - pw // 2, CY + 62 * SCALE
    draw_chamfered_card(d, px, py, pw, ph, 4 * SCALE, (10, 20, 34, 225), (0, 240, 255, 165), 1.1 * SCALE)
    dr = 2.6 * SCALE
    d.ellipse([px + 10 * SCALE - dr, py + ph // 2 - dr, px + 10 * SCALE + dr, py + ph // 2 + dr], fill=(27, 255, 156, 255))
    d.text((px + 17 * SCALE, py + 3.5 * SCALE), "AVALON NANO 3S", font=f_pill, fill=(224, 230, 255, 255))
    return crop_to_240(scr)


def tpl_splash_minimal():
    scr = make_screen_canvas()
    draw_minimal_bg(scr, dark=False)
    r_ring, w_ring = 107 * SCALE, 6 * SCALE
    rim_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(rim_l), CX, CY, r_ring, 0, 360, w_ring + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    scr.alpha_composite(rim_l)
    tr_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(tr_l), CX, CY, r_ring, 0, 360, w_ring, (15, 23, 42, 24), round_caps=True)
    scr.alpha_composite(tr_l)

    mr = 49 * SCALE
    mcy = CY - 24 * SCALE
    med = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    md = ImageDraw.Draw(med, "RGBA")
    md.ellipse([CX - mr, mcy - mr + 3 * SCALE, CX + mr, mcy + mr + 3 * SCALE], fill=(115, 135, 170, 55))
    md.ellipse([CX - mr, mcy - mr, CX + mr, mcy + mr], fill=(255, 255, 255, 195), outline=(255, 255, 255, 250), width=int(1.6 * SCALE))
    scr.alpha_composite(med)

    rac = resize_raccoon(80 * SCALE)
    r_sh = Image.composite(Image.new("RGBA", rac.size, (115, 135, 170, 55)), Image.new("RGBA", rac.size, (0, 0, 0, 0)), rac.split()[3]).filter(ImageFilter.GaussianBlur(radius=2 * SCALE))
    rx, ry = CX - rac.width // 2, mcy - rac.height // 2
    scr.alpha_composite(r_sh, (rx, ry + int(1.8 * SCALE)))
    scr.alpha_composite(rac, (rx, ry))

    d = ImageDraw.Draw(scr, "RGBA")
    f_title = get_font(SEGOE_BOLD, 24 * SCALE)
    f_pill = get_font(SEGOE_BOLD, 8.2 * SCALE)
    draw_centered_text(d, CX, CY + 24 * SCALE, "MUJINA", f_title, (30, 41, 59, 255))
    bar_w, bar_h = 34 * SCALE, 3 * SCALE
    d.rounded_rectangle([CX - bar_w // 2, CY + 57 * SCALE, CX + bar_w // 2, CY + 57 * SCALE + bar_h], radius=bar_h // 2, fill=(0, 102, 204, 255))

    pw, ph = 108 * SCALE, 18 * SCALE
    px, py = CX - pw // 2, CY + 67 * SCALE
    draw_glass_card(scr, px, py, pw, ph, 9 * SCALE, (255, 255, 255, 195), (255, 255, 255, 250), (115, 135, 170, 55), rim_w=1.2)
    d = ImageDraw.Draw(scr, "RGBA")
    dr = 2.8 * SCALE
    d.ellipse([px + 11 * SCALE - dr, py + ph // 2 - dr, px + 11 * SCALE + dr, py + ph // 2 + dr], fill=(16, 185, 129, 255))
    d.text((px + 19 * SCALE, py + 3.2 * SCALE), "Avalon Nano 3S", font=f_pill, fill=(92, 111, 132, 255))
    return crop_to_240(scr)


def tpl_splash_parasite():
    scr = make_screen_canvas()
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(0, 0, 0, 255))
    white = (255, 255, 255, 255)
    dim = (155, 155, 155, 255)
    black = (0, 0, 0, 255)

    f_hdr = get_font(COURIER_BOLD, 7.8 * SCALE)
    f_title = get_font(COURIER_BOLD, 19.5 * SCALE)
    f_bar = get_font(COURIER_BOLD, 8.2 * SCALE)
    draw_centered_text(d, CX, CY - 86 * SCALE, "[ SYS // INIT ]", f_hdr, dim)

    rac, _ = make_raccoon_parasite(86 * SCALE)
    scr.alpha_composite(rac, (CX - rac.width // 2, CY - 31 * SCALE - rac.height // 2))

    d = ImageDraw.Draw(scr, "RGBA")
    border_col = (212, 212, 212, 255)
    bw_1px = max(1, int(round(1.1 * SCALE)))
    bw, bh = 148 * SCALE, 31 * SCALE
    bx, by = CX - bw // 2, CY + 25 * SCALE
    d.rectangle([bx, by, bx + bw, by + bh], fill=black, outline=border_col, width=bw_1px)
    draw_centered_text(d, CX, by + 6 * SCALE, "// MUJINA //", f_title, white)

    sw, sh = 118 * SCALE, 15 * SCALE
    sx, sy = CX - sw // 2, CY + 64 * SCALE
    d.rectangle([sx, sy, sx + sw, sy + sh], fill=white)
    draw_centered_text(d, CX, sy + 2.8 * SCALE, "NANO3S : ONLINE", f_bar, black)
    return crop_to_240(scr)


def tpl_live_cyber():
    scr = make_screen_canvas()
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    cyan = (0, 240, 255, 255)
    mag = (255, 46, 151, 255)
    card_bg = (10, 15, 28, 225)
    dim = (118, 152, 182, 255)
    white = (255, 255, 255, 255)

    f_brand = get_font(ORBITRON, 9.5 * SCALE)
    f_unit = get_font(ORBITRON, 9.5 * SCALE)
    f_lbl = get_font(SPACE_MONO, 7.0 * SCALE)

    b1 = d.textbbox((0, 0), "//", font=f_brand)
    b2 = d.textbbox((0, 0), "MUJINA", font=f_brand)
    w1, w2 = b1[2] - b1[0], b2[2] - b2[0]
    gap_hdr = 4 * SCALE
    hx = CX - (w1 + gap_hdr + w2) // 2
    hy = CY - 84 * SCALE
    d.text((hx, hy), "//", font=f_brand, fill=mag)
    d.text((hx + w1 + gap_hdr, hy), "MUJINA", font=f_brand, fill=white)

    hero_w, hero_h = 136 * SCALE, 52 * SCALE
    hero_x, hero_y = CX - hero_w // 2, CY - 44 * SCALE
    draw_chamfered_card(d, hero_x, hero_y, hero_w, hero_h, 8 * SCALE, card_bg, (0, 240, 255, 165), 1.4 * SCALE, left_neon_color=cyan)
    draw_centered_text(d, CX, hero_y + 36 * SCALE, "TH/S", f_unit, cyan)

    tile_w, tile_h = 64 * SCALE, 32 * SCALE
    t_gap = 6 * SCALE
    tx1, tx2 = CX - tile_w - t_gap // 2, CX + t_gap // 2
    ty = CY + 14 * SCALE
    draw_chamfered_card(d, tx1, ty, tile_w, tile_h, 5 * SCALE, card_bg, (255, 176, 0, 145), 1.1 * SCALE)
    draw_centered_text(d, tx1 + tile_w // 2, ty + 3.5 * SCALE, "ASIC TEMP", f_lbl, dim)
    draw_chamfered_card(d, tx2, ty, tile_w, tile_h, 5 * SCALE, card_bg, (0, 240, 255, 145), 1.1 * SCALE)
    draw_centered_text(d, tx2 + tile_w // 2, ty + 3.5 * SCALE, "PWR DRAW", f_lbl, dim)
    return crop_to_240(scr)


def tpl_live_minimal():
    scr = make_screen_canvas()
    draw_minimal_bg(scr, dark=False)
    r_outer, w_outer = 104 * SCALE, 7.5 * SCALE
    r_inner, w_inner = 91 * SCALE, 5.5 * SCALE

    rim_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rim_l)
    draw_smooth_arc(rd, CX, CY, r_outer, 145, 395, w_outer + 2 * SCALE, (255, 255, 255, 195), round_caps=True)
    draw_smooth_arc(rd, CX, CY, r_inner, 115, 200, w_inner + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    draw_smooth_arc(rd, CX, CY, r_inner, 340, 425, w_inner + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    scr.alpha_composite(rim_l)

    tr_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    td = ImageDraw.Draw(tr_l)
    draw_smooth_arc(td, CX, CY, r_outer, 145, 395, w_outer, (15, 23, 42, 24), round_caps=True)
    draw_smooth_arc(td, CX, CY, r_inner, 115, 200, w_inner, (15, 23, 42, 24), round_caps=True)
    draw_smooth_arc(td, CX, CY, r_inner, 340, 425, w_inner, (15, 23, 42, 24), round_caps=True)
    scr.alpha_composite(tr_l)

    glass_fill = (255, 255, 255, 182)
    glass_pill = (255, 255, 255, 208)
    glass_rim = (255, 255, 255, 248)
    shadow_col = (115, 135, 170, 50)

    draw_glass_card(scr, CX - 57 * SCALE, CY - 74 * SCALE, 114 * SCALE, 18 * SCALE, 9 * SCALE, glass_pill, glass_rim, shadow_col, rim_w=1.2)
    draw_glass_card(scr, CX - 66 * SCALE, CY - 46 * SCALE, 132 * SCALE, 56 * SCALE, 16 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.5)
    pw, ph = 62 * SCALE, 32 * SCALE
    pgap = 6 * SCALE
    px1, px2 = CX - pw - pgap // 2, CX + pgap // 2
    py = CY + 17 * SCALE
    draw_glass_card(scr, px1, py, pw, ph, 11 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)
    draw_glass_card(scr, px2, py, pw, ph, 11 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)

    d = ImageDraw.Draw(scr, "RGBA")
    f_unit = get_font(SEGOE_BOLD, 9.5 * SCALE)
    f_lbl = get_font(SEGOE_REG, 7.5 * SCALE)
    draw_centered_text(d, CX, CY - 46 * SCALE + 37 * SCALE, "TH/s", f_unit, (78, 95, 118, 255))
    draw_centered_text(d, px1 + pw // 2, py + 3 * SCALE, "Temp", f_lbl, (78, 95, 118, 255))
    draw_centered_text(d, px2 + pw // 2, py + 3 * SCALE, "Power", f_lbl, (78, 95, 118, 255))
    return crop_to_240(scr)


def tpl_live_parasite():
    scr = make_screen_canvas()
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(0, 0, 0, 255))
    white = (255, 255, 255, 255)
    border_col = (212, 212, 212, 255)
    bw_1px = max(1, int(round(1.1 * SCALE)))
    dim = (165, 165, 165, 255)
    f_unit = get_font(COURIER_BOLD, 9.5 * SCALE)
    f_lbl = get_font(COURIER, 7.2 * SCALE)

    # Parasite.space Pool Hashrate style outer dial segments + thin inner rim
    seg_bg = (46, 46, 46, 255)
    for k in range(9):
        a1 = 135.0 + k * 30.0 + 1.0
        a2 = a1 + 28.0
        draw_wedge_arc(d, CX, CY, 103.5 * SCALE, a1, a2, 11.0 * SCALE, seg_bg)
    draw_wedge_arc(d, CX, CY, 97.5 * SCALE, 135.0, 405.0, 1.1 * SCALE, (112, 112, 112, 255))

    d.text((CX - 66 * SCALE, CY + 69 * SCALE), "0", font=f_lbl, fill=dim)
    d.text((CX + 60 * SCALE, CY + 69 * SCALE), "7", font=f_lbl, fill=dim)

    hero_w, hero_h = 136 * SCALE, 52 * SCALE
    hero_x, hero_y = CX - hero_w // 2, CY - 48 * SCALE
    d.rectangle([hero_x, hero_y, hero_x + hero_w, hero_y + hero_h], outline=border_col, width=bw_1px)
    draw_centered_text(d, CX, hero_y + 36 * SCALE, "TH/S", f_unit, dim)

    tw, th = 65 * SCALE, 33 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 11 * SCALE
    d.rectangle([tx1, ty, tx1 + tw, ty + th], outline=border_col, width=bw_1px)
    d.rectangle([tx2, ty, tx2 + tw, ty + th], outline=border_col, width=bw_1px)
    draw_centered_text(d, tx2 + tw // 2, ty + 4.5 * SCALE, "POWER", f_lbl, dim)
    return crop_to_240(scr)


def tpl_tuner_cyber():
    scr = make_screen_canvas()
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    cyan = (0, 240, 255, 255)
    mag = (255, 46, 151, 255)
    green = (27, 255, 156, 255)
    hot_red = (255, 46, 85, 255)
    card_bg = (10, 15, 28, 225)
    dim = (118, 152, 182, 255)

    f_hdr = get_font(ORBITRON, 9.2 * SCALE)
    f_lbl = get_font(SPACE_MONO, 6.8 * SCALE)
    b1 = d.textbbox((0, 0), "//", font=f_hdr)
    b2 = d.textbbox((0, 0), "AUTO TUNER", font=f_hdr)
    w1, w2 = b1[2] - b1[0], b2[2] - b2[0]
    hx = CX - (w1 + 4 * SCALE + w2) // 2
    hy = CY - 80 * SCALE
    d.text((hx, hy), "//", font=f_hdr, fill=mag)
    d.text((hx + w1 + 4 * SCALE, hy), "AUTO TUNER", font=f_hdr, fill=cyan)

    hw, hh = 144 * SCALE, 46 * SCALE
    hx0, hy0 = CX - hw // 2, CY - 58 * SCALE
    draw_chamfered_card(d, hx0, hy0, hw, hh, 7 * SCALE, card_bg, (255, 46, 151, 170), 1.3 * SCALE, left_neon_color=mag)
    draw_centered_text(d, CX, hy0 + 4.5 * SCALE, "TARGET PROFILE", f_lbl, dim)

    tw, th = 66 * SCALE, 38 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 19 * SCALE
    draw_chamfered_card(d, tx1, ty, tw, th, 5 * SCALE, card_bg, (27, 255, 156, 145), 1.1 * SCALE, left_neon_color=green)
    draw_chamfered_card(d, tx2, ty, tw, th, 5 * SCALE, card_bg, (255, 46, 85, 165), 1.1 * SCALE, left_neon_color=hot_red)
    return crop_to_240(scr)


def tpl_tuner_minimal():
    scr = make_screen_canvas()
    draw_minimal_bg(scr, dark=False)
    r_track, w_track = 105.5 * SCALE, 11 * SCALE
    rim_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(rim_l), CX, CY, r_track, 0, 360, w_track + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    scr.alpha_composite(rim_l)
    tr_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    draw_smooth_arc(ImageDraw.Draw(tr_l), CX, CY, r_track, 0, 360, w_track, (15, 23, 42, 22), round_caps=True)
    scr.alpha_composite(tr_l)

    glass_fill = (255, 255, 255, 188)
    glass_pill = (255, 255, 255, 212)
    glass_rim = (255, 255, 255, 248)
    shadow_col = (115, 135, 170, 50)

    d = ImageDraw.Draw(scr, "RGBA")
    draw_centered_text(d, CX, CY - 79 * SCALE, "AUTO TUNER", get_font(SEGOE_BOLD, 9.2 * SCALE), (45, 62, 80, 255))
    draw_glass_card(scr, CX - 72 * SCALE, CY - 59 * SCALE, 144 * SCALE, 46 * SCALE, 13 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.4)
    draw_glass_card(scr, CX - 64 * SCALE, CY - 6 * SCALE, 128 * SCALE, 18 * SCALE, 9 * SCALE, glass_pill, glass_rim, shadow_col, rim_w=1.2)
    tw, th = 66 * SCALE, 38 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 19 * SCALE
    draw_glass_card(scr, tx1, ty, tw, th, 11 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)
    draw_glass_card(scr, tx2, ty, tw, th, 11 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)

    d = ImageDraw.Draw(scr, "RGBA")
    draw_centered_text(d, CX, CY - 59 * SCALE + 4.5 * SCALE, "Target Profile", get_font(SEGOE_REG, 7.2 * SCALE), (78, 95, 118, 255))
    return crop_to_240(scr)


def tpl_tuner_parasite():
    scr = make_screen_canvas()
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(0, 0, 0, 255))
    white = (255, 255, 255, 255)
    dim = (165, 165, 165, 255)
    f_hdr = get_font(COURIER_BOLD, 8.8 * SCALE)
    f_lbl = get_font(COURIER, 6.8 * SCALE)

    border_col = (212, 212, 212, 255)
    bw_1px = max(1, int(round(1.1 * SCALE)))
    draw_centered_text(d, CX, CY - 80 * SCALE, "// AUTO_TUNER //", f_hdr, white)
    hw, hh = 142 * SCALE, 45 * SCALE
    hx0, hy0 = CX - hw // 2, CY - 59 * SCALE
    d.rectangle([hx0, hy0, hx0 + hw, hy0 + hh], outline=border_col, width=bw_1px)
    draw_centered_text(d, CX, hy0 + 5 * SCALE, "TARGET PROFILE:", f_lbl, dim)

    tw, th = 66 * SCALE, 38 * SCALE
    gap = 6 * SCALE
    tx1 = CX - tw - gap // 2
    ty = CY + 18 * SCALE
    d.rectangle([tx1, ty, tx1 + tw, ty + th], outline=border_col, width=bw_1px)
    return crop_to_240(scr)


def tpl_ip_cyber():
    scr = make_screen_canvas()
    draw_cyberpunk_bg(scr)
    d = ImageDraw.Draw(scr, "RGBA")
    cyan = (0, 240, 255, 255)
    mag = (255, 46, 151, 255)
    green = (27, 255, 156, 255)
    card_bg = (10, 15, 28, 225)
    dim = (118, 152, 182, 255)

    draw_wedge_arc(d, CX, CY, 109 * SCALE, 248, 292, 2.2 * SCALE, (0, 240, 255, 110))
    draw_wedge_arc(d, CX, CY, 109 * SCALE, 68, 112, 2.2 * SCALE, (0, 240, 255, 110))

    f_hdr = get_font(ORBITRON, 9.5 * SCALE)
    f_lbl = get_font(SPACE_MONO, 6.8 * SCALE)
    b1 = d.textbbox((0, 0), "//", font=f_hdr)
    b2 = d.textbbox((0, 0), "NETWORK", font=f_hdr)
    w1, w2 = b1[2] - b1[0], b2[2] - b2[0]
    hx = CX - (w1 + 4 * SCALE + w2) // 2
    hy = CY - 82 * SCALE
    d.text((hx, hy), "//", font=f_hdr, fill=mag)
    d.text((hx + w1 + 4 * SCALE, hy), "NETWORK", font=f_hdr, fill=cyan)

    hw, hh = 166 * SCALE, 52 * SCALE
    hx0, hy0 = CX - hw // 2, CY - 40 * SCALE
    draw_chamfered_card(d, hx0, hy0, hw, hh, 8 * SCALE, card_bg, (0, 240, 255, 170), 1.4 * SCALE, left_neon_color=cyan)
    draw_centered_text(d, CX, hy0 + 5 * SCALE, "IPV4 ADDRESS // DHCP", f_lbl, dim)

    tw, th = 66 * SCALE, 36 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 20 * SCALE
    draw_chamfered_card(d, tx1, ty, tw, th, 5 * SCALE, card_bg, (27, 255, 156, 150), 1.1 * SCALE, left_neon_color=green)
    draw_centered_text(d, tx1 + tw // 2, ty + 4 * SCALE, "SIGNAL", f_lbl, dim)
    draw_chamfered_card(d, tx2, ty, tw, th, 5 * SCALE, card_bg, (255, 46, 151, 150), 1.1 * SCALE, left_neon_color=mag)
    draw_centered_text(d, tx2 + tw // 2, ty + 4 * SCALE, "WEB UI", f_lbl, dim)
    return crop_to_240(scr)


def tpl_ip_minimal():
    scr = make_screen_canvas()
    draw_minimal_bg(scr, dark=False)
    r_ring, w_ring = 107 * SCALE, 6.5 * SCALE
    rim_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rim_l)
    draw_smooth_arc(rd, CX, CY, r_ring, 125, 235, w_ring + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    draw_smooth_arc(rd, CX, CY, r_ring, 305, 415, w_ring + 1.8 * SCALE, (255, 255, 255, 195), round_caps=True)
    scr.alpha_composite(rim_l)

    tr_l = Image.new("RGBA", (TOTAL_SIZE, TOTAL_SIZE), (0, 0, 0, 0))
    td = ImageDraw.Draw(tr_l)
    draw_smooth_arc(td, CX, CY, r_ring, 125, 235, w_ring, (15, 23, 42, 24), round_caps=True)
    draw_smooth_arc(td, CX, CY, r_ring, 305, 415, w_ring, (15, 23, 42, 24), round_caps=True)
    scr.alpha_composite(tr_l)

    glass_fill = (255, 255, 255, 188)
    glass_pill = (255, 255, 255, 212)
    glass_rim = (255, 255, 255, 248)
    shadow_col = (115, 135, 170, 50)

    d = ImageDraw.Draw(scr, "RGBA")
    draw_centered_text(d, CX, CY - 82 * SCALE, "NETWORK", get_font(SEGOE_BOLD, 9.5 * SCALE), (45, 62, 80, 255))
    draw_glass_card(scr, CX - 59 * SCALE, CY - 63 * SCALE, 118 * SCALE, 17 * SCALE, 8.5 * SCALE, glass_pill, glass_rim, shadow_col, rim_w=1.2)
    draw_glass_card(scr, CX - 83 * SCALE, CY - 40 * SCALE, 166 * SCALE, 52 * SCALE, 14 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.4)
    tw, th = 66 * SCALE, 36 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 20 * SCALE
    draw_glass_card(scr, tx1, ty, tw, th, 10 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)
    draw_glass_card(scr, tx2, ty, tw, th, 10 * SCALE, glass_fill, glass_rim, shadow_col, rim_w=1.2)

    d = ImageDraw.Draw(scr, "RGBA")
    f_lbl = get_font(SEGOE_REG, 7.2 * SCALE)
    draw_centered_text(d, CX, CY - 40 * SCALE + 5 * SCALE, "IPv4 Address (DHCP)", f_lbl, (78, 95, 118, 255))
    draw_centered_text(d, tx1 + tw // 2, ty + 4 * SCALE, "Signal", f_lbl, (78, 95, 118, 255))
    draw_centered_text(d, tx2 + tw // 2, ty + 4 * SCALE, "Dashboard", f_lbl, (78, 95, 118, 255))
    return crop_to_240(scr)


def tpl_ip_parasite():
    scr = make_screen_canvas()
    d = ImageDraw.Draw(scr, "RGBA")
    d.rectangle([0, 0, TOTAL_SIZE, TOTAL_SIZE], fill=(0, 0, 0, 255))
    white = (255, 255, 255, 255)
    dim = (165, 165, 165, 255)
    f_hdr = get_font(COURIER_BOLD, 8.8 * SCALE)
    f_lbl = get_font(COURIER, 6.8 * SCALE)
    border_col = (212, 212, 212, 255)
    bw_1px = max(1, int(round(1.1 * SCALE)))
    draw_centered_text(d, CX, CY - 82 * SCALE, "// NET_CONFIG //", f_hdr, white)

    hw, hh = 166 * SCALE, 52 * SCALE
    hx0, hy0 = CX - hw // 2, CY - 40 * SCALE
    d.rectangle([hx0, hy0, hx0 + hw, hy0 + hh], outline=border_col, width=bw_1px)
    draw_centered_text(d, CX, hy0 + 5 * SCALE, "IPV4_ADDR [DHCP]:", f_lbl, dim)

    tw, th = 66 * SCALE, 36 * SCALE
    gap = 6 * SCALE
    tx1, tx2 = CX - tw - gap // 2, CX + gap // 2
    ty = CY + 20 * SCALE
    d.rectangle([tx1, ty, tx1 + tw, ty + th], outline=border_col, width=bw_1px)
    draw_centered_text(d, tx1 + tw // 2, ty + 4 * SCALE, "RSSI", f_lbl, dim)
    d.rectangle([tx2, ty, tx2 + tw, ty + th], outline=border_col, width=bw_1px)
    draw_centered_text(d, tx2 + tw // 2, ty + 4 * SCALE, "HTTPD", f_lbl, dim)
    return crop_to_240(scr)


def tpl_parasite_logo():
    return crop_to_240(render_s5_parasite_logo(with_bezel=False))


def rgb_img_to_rgb565_u16(img):
    arr = np.array(img, dtype=np.uint16)
    r5 = arr[:, :, 0] >> 3
    g6 = arr[:, :, 1] >> 2
    b5 = arr[:, :, 2] >> 3
    return ((r5 << 11) | (g6 << 5) | b5).reshape(-1)


def generate_ui_assets_h(out_path, save_previews_dir=None):
    templates = [
        ("bg_splash_cyber", tpl_splash_cyber()),
        ("bg_splash_minimal", tpl_splash_minimal()),
        ("bg_splash_parasite", tpl_splash_parasite()),
        ("bg_live_cyber", tpl_live_cyber()),
        ("bg_live_minimal", tpl_live_minimal()),
        ("bg_live_parasite", tpl_live_parasite()),
        ("bg_tuner_cyber", tpl_tuner_cyber()),
        ("bg_tuner_minimal", tpl_tuner_minimal()),
        ("bg_tuner_parasite", tpl_tuner_parasite()),
        ("bg_ip_cyber", tpl_ip_cyber()),
        ("bg_ip_minimal", tpl_ip_minimal()),
        ("bg_ip_parasite", tpl_ip_parasite()),
        ("bg_parasite_logo", tpl_parasite_logo()),
    ]
    lines = [
        "/* Auto-generated 240x240 RGB565 background templates for Avalon Nano 3S */",
        "#ifndef UI_ASSETS_H",
        "#define UI_ASSETS_H",
        "#include <stdint.h>",
        "",
    ]
    for name, img in templates:
        if save_previews_dir is not None:
            os.makedirs(save_previews_dir, exist_ok=True)
            img.save(os.path.join(save_previews_dir, f"tpl_{name}.png"))
        vals = rgb_img_to_rgb565_u16(img)
        lines.append(f"static const uint16_t {name}[57600] = {{")
        for i in range(0, len(vals), 16):
            chunk = ",".join(f"0x{v:04x}" for v in vals[i:i + 16])
            lines.append(f"    {chunk},")
        lines.append("};")
        lines.append("")
    lines.append("#endif /* UI_ASSETS_H */")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines))
    print(f"Wrote {out_path} ({os.path.getsize(out_path):,} bytes)")


# ==============================================================================
# BITMAP FONT GENERATOR (4x SUPERSAMPLED LANCZOS 8-BIT ALPHA GLYPHS -> ui_fonts.h)
# ==============================================================================

FONT_CHARS = [chr(c) for c in range(32, 127)] + ["\u00b0", "\u2022"]  # ASCII 32..126 + ° + •

FONT_SPECS = [
    ("font_orbitron_29", ORBITRON, 29.0),
    ("font_orbitron_19", ORBITRON, 19.0),
    ("font_orbitron_12", ORBITRON, 11.8),
    ("font_space_8", SPACE_MONO, 8.2),
    ("font_bahn_33", BAHNSCHRIFT, 33.0),
    ("font_bahn_22", BAHNSCHRIFT, 22.0),
    ("font_bahn_13", BAHNSCHRIFT, 13.5),
    ("font_segoe_8", SEGOE_BOLD, 8.4),
    ("font_courier_31", COURIER_BOLD, 31.0),
    ("font_courier_20", COURIER_BOLD, 20.0),
    ("font_courier_13", COURIER_BOLD, 12.8),
    ("font_courier_8", COURIER_BOLD, 8.4),
]


def generate_ui_fonts_h(out_path):
    SS = 4
    lines = [
        "/* Auto-generated anti-aliased 8-bit alpha glyph fonts for 240x240 RGB565 */",
        "#ifndef UI_FONTS_H",
        "#define UI_FONTS_H",
        "#include <stdint.h>",
        "",
        "typedef struct {",
        "    uint16_t codepoint;",
        "    uint8_t width;",
        "    uint8_t height;",
        "    int8_t x_off;",
        "    int8_t y_off;",
        "    uint8_t advance;",
        "    uint32_t data_offset;",
        "} ui_glyph_t;",
        "",
        "typedef struct {",
        "    uint8_t line_height;",
        "    uint16_t glyph_count;",
        "    const ui_glyph_t *glyphs;",
        "    const uint8_t *bitmap;",
        "} ui_font_t;",
        "",
    ]

    for sym, ttf_path, pt_size in FONT_SPECS:
        font_ss = get_font(ttf_path, pt_size * SS)
        glyphs = []
        bitmap_bytes = bytearray()

        # Baseline reference across sample string
        pad = 24
        for ch in FONT_CHARS:
            # Render at 4x onto oversized L canvas
            canvas = Image.new("L", (280, 280), 0)
            cd = ImageDraw.Draw(canvas)
            ox, oy = 40, 40
            cd.text((ox, oy), ch, font=font_ss, fill=255)
            # Get advance width via length
            adv = max(1, int(round(cd.textlength(ch, font=font_ss) / SS)))

            # Downsample 4x -> 1x so coordinates are aligned to grid multiple of 4
            cropped_1x = canvas.resize((280 // SS, 280 // SS), Image.Resampling.LANCZOS)
            arr = np.array(cropped_1x, dtype=np.uint8)
            nz = np.argwhere(arr > 4)
            if len(nz) == 0:
                glyphs.append((ord(ch), 0, 0, 0, 0, adv, len(bitmap_bytes)))
            else:
                y0, x0 = nz.min(axis=0)
                y1, x1 = nz.max(axis=0)
                sub = arr[y0:y1 + 1, x0:x1 + 1]
                h, w = sub.shape
                x_off = int(x0 - (ox // SS))
                y_off = int(y0 - (oy // SS))
                data_off = len(bitmap_bytes)
                bitmap_bytes.extend(sub.tobytes())
                glyphs.append((ord(ch), w, h, x_off, y_off, adv, data_off))

        lines.append(f"static const uint8_t {sym}_bitmap[] = {{")
        if len(bitmap_bytes) == 0:
            lines.append("    0")
        else:
            for i in range(0, len(bitmap_bytes), 20):
                lines.append("    " + ",".join(str(b) for b in bitmap_bytes[i:i + 20]) + ",")
        lines.append("};")
        lines.append(f"static const ui_glyph_t {sym}_glyphs[] = {{")
        for g in glyphs:
            lines.append(f"    {{{g[0]}, {g[1]}, {g[2]}, {g[3]}, {g[4]}, {g[5]}, {g[6]}u}},")
        lines.append("};")
        lines.append(f"static const ui_font_t {sym} = {{{int(round(pt_size * 1.2))}, {len(glyphs)}u, {sym}_glyphs, {sym}_bitmap}};")
        lines.append("")

    lines.append("#endif /* UI_FONTS_H */")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines))
    print(f"Wrote {out_path} ({os.path.getsize(out_path):,} bytes)")



def main():
    parser = argparse.ArgumentParser(description="Generate Nano 3S LCD C assets and bitmap fonts.")
    parser.add_argument("--assets-out", type=Path, default=DEFAULT_ASSETS_H)
    parser.add_argument("--fonts-out", type=Path, default=DEFAULT_FONTS_H)
    parser.add_argument("--save-previews", type=Path, default=None)
    args = parser.parse_args()
    generate_ui_assets_h(str(args.assets_out), str(args.save_previews) if args.save_previews else None)
    generate_ui_fonts_h(str(args.fonts_out))


if __name__ == "__main__":
    main()
