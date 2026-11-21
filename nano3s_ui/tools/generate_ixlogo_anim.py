import os
import glob
import zlib
import subprocess
from PIL import Image
import numpy as np

def generate():
    mp4_path = r"C:\Users\grant\.gemini\antigravity\brain\fa08a51d-7d2a-471f-accf-2797da253ec2\.user_uploaded\media_1795230270857_3f37042e.mp4"
    scratch_dir = r"C:\Users\grant\.gemini\antigravity\brain\fa08a51d-7d2a-471f-accf-2797da253ec2\scratch\frames_240_full"
    os.makedirs(scratch_dir, exist_ok=True)

    # 1. Extract 150 frames (15 FPS for 10.0s) from raw 720p MP4
    raw_dir = r"C:\Users\grant\.gemini\antigravity\brain\fa08a51d-7d2a-471f-accf-2797da253ec2\scratch\all_raw_15fps"
    os.makedirs(raw_dir, exist_ok=True)
    raw_files = sorted(glob.glob(os.path.join(raw_dir, "raw_*.png")))[:150]
    if len(raw_files) != 150:
        print("Extracting 150 raw frames at 15 fps...")
        cmd = [
            "ffmpeg", "-y", "-i", mp4_path,
            "-r", "15",
            os.path.join(raw_dir, "raw_%04d.png")
        ]
        subprocess.run(cmd, check=True, capture_output=True)
        raw_files = sorted(glob.glob(os.path.join(raw_dir, "raw_*.png")))[:150]

    num_frames = len(raw_files)
    if num_frames != 150:
        raise ValueError(f"Expected 150 frames, found {num_frames}")
    print(f"Loaded {num_frames} raw frames")

    # Scale and center so all text, logo, and animation stay 100% inside R <= 108.2 px
    # Composition center in 1280x720 is (637.0, 361.0).
    # S = 0.255 maps the composition width (609px) to ~155px, and height (603px) to ~154px.
    # Max radius to furthest text corners is exactly 108.19 px (well inside R=114 bezel).
    S = 0.255
    scaled_w = int(round(1280 * S))
    scaled_h = int(round(720 * S))
    paste_x = int(round(120.0 - 637.0 * S))
    paste_y = int(round(120.0 - 361.0 * S))

    # 2. Process frames: circular vignette mask + black noise gate + subtle electric rim
    cy, cx = 120.0, 120.0
    Y, X = np.ogrid[:240, :240]
    dist = np.sqrt((X - cx)**2 + (Y - cy)**2)

    # Circular mask: full brightness up to r=114, smooth roll-off to 0 at r=117.5
    mask = np.clip((117.5 - dist) / 3.5, 0.0, 1.0)[:, :, np.newaxis]
    # Subtle electric rim: Gaussian profile around r=117.0 (matching IxTech electric violet-cyan glow)
    rim_profile = np.exp(-0.5 * ((dist - 117.0) / 0.75)**2)[:, :, np.newaxis]
    rim_color = np.array([160.0, 100.0, 240.0], dtype=np.float32)

    processed = []
    for f in raw_files:
        im = Image.open(f).resize((scaled_w, scaled_h), Image.Resampling.LANCZOS)
        canvas = Image.new("RGB", (240, 240), (0, 0, 0))
        canvas.paste(im, (paste_x, paste_y))
        arr = np.array(canvas, dtype=np.float32)

        # Noise gate: crush background video compression artifacts to pure black
        max_c = np.max(arr, axis=2, keepdims=True)
        noise_gate = np.clip((max_c - 6.0) / 10.0, 0.0, 1.0)
        arr_clean = arr * noise_gate * mask
        # Add subtle electric rim
        arr_rim = arr_clean + rim_profile * rim_color * 0.85
        # Strict zeroing outside circle screen radius (r > 118.0)
        arr_rim[dist > 118.0] = 0.0
        processed.append(arr_rim)

    # 3. Seamless loop cross-fade on last 10 frames towards frame 0
    fade_len = 10
    f0 = processed[0]
    for idx in range(fade_len):
        frame_idx = num_frames - fade_len + idx
        alpha = (idx + 1) / (fade_len + 1)
        processed[frame_idx] = (1.0 - alpha) * processed[frame_idx] + alpha * f0

    # 4. Global 32-color quantization across all frames
    comp_w = 240 * 15
    comp_h = 240 * 10
    comp = Image.new("RGB", (comp_w, comp_h))
    for i, arr in enumerate(processed):
        im = Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))
        comp.paste(im, ((i % 15) * 240, (i // 15) * 240))

    num_colors = 32
    pal_img = comp.quantize(colors=num_colors, method=Image.Quantize.FASTOCTREE)
    raw_pal = bytearray(pal_img.getpalette()[:num_colors * 3])

    # Ensure index 0 is strictly (0, 0, 0) for pure black
    raw_pal[0:3] = b"\x00\x00\x00"
    pal_img.putpalette(raw_pal)

    # Convert palette to RGB565 uint16
    palette_rgb565 = []
    for i in range(num_colors):
        r = raw_pal[i * 3 + 0]
        g = raw_pal[i * 3 + 1]
        b = raw_pal[i * 3 + 2]
        r5 = (r >> 3) & 0x1F
        g6 = (g >> 2) & 0x3F
        b5 = (b >> 3) & 0x1F
        u16 = (r5 << 11) | (g6 << 5) | b5
        palette_rgb565.append(u16)
    palette_rgb565[0] = 0x0000

    print("Quantizing all frames to 32 colors with strict circular zeroing...")
    raw_payload = bytearray()
    for arr in processed:
        im = Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))
        q = im.quantize(palette=pal_img, dither=Image.Dither.NONE)
        q_arr = np.array(q, dtype=np.uint8)
        # Strict zeroing: guarantee every pixel outside circular screen is index 0 (black 0x0000)
        q_arr[dist > 118.0] = 0
        raw_payload.extend(q_arr.tobytes())

    uncompressed_len = len(raw_payload)
    print(f"Uncompressed payload: {uncompressed_len:,} bytes ({uncompressed_len / (1024 * 1024):.2f} MB)")

    # 5. Raw DEFLATE compression (wbits = -15)
    print("Compressing payload with raw DEFLATE level 9...")
    comp_obj = zlib.compressobj(level=9, method=zlib.DEFLATED, wbits=-15)
    deflate_bytes = comp_obj.compress(bytes(raw_payload)) + comp_obj.flush()
    print(f"Raw DEFLATE compressed size: {len(deflate_bytes):,} bytes ({len(deflate_bytes) / 1024:.1f} KB)")

    # 6. Verify decompression with zlib
    decomp = zlib.decompress(deflate_bytes, -15)
    assert decomp == bytes(raw_payload), "Decompression self-check failed!"
    print("Self-check passed: DEFLATE decompress matches raw payload exactly!")

    # 7. Write C header ixlogo_anim.h
    out_h = r"D:\github\mujina-v5.2-staging\nano3s_ui\c_driver\ixlogo_anim.h"
    print(f"Writing {out_h}...")
    with open(out_h, "w", encoding="utf-8") as f:
        f.write("/* Auto-generated IxTech animated logo assets (240x240 circular, 15 fps, 10.0s, 32 colors) */\n")
        f.write("#ifndef IXLOGO_ANIM_H\n")
        f.write("#define IXLOGO_ANIM_H\n\n")
        f.write("#include <stdint.h>\n\n")
        f.write("#define IXLOGO_FRAME_W       240\n")
        f.write("#define IXLOGO_FRAME_H       240\n")
        f.write(f"#define IXLOGO_NUM_FRAMES    {num_frames}\n")
        f.write("#define IXLOGO_FRAME_BYTES   (IXLOGO_FRAME_W * IXLOGO_FRAME_H)\n")
        f.write(f"#define IXLOGO_TOTAL_BYTES   {uncompressed_len}\n")
        f.write("#define IXLOGO_FRAME_MS      66 /* 15 FPS */\n\n")

        # Palette
        f.write(f"static const uint16_t ixlogo_palette[{num_colors}] = {{\n    ")
        for i, val in enumerate(palette_rgb565):
            f.write(f"0x{val:04x}, ")
            if (i + 1) % 8 == 0:
                f.write("\n    ")
        f.write("\n};\n\n")

        # Compressed byte array
        f.write(f"static const uint8_t ixlogo_deflate[{len(deflate_bytes)}] = {{\n")
        for i in range(0, len(deflate_bytes), 16):
            chunk = ", ".join(f"0x{b:02x}" for b in deflate_bytes[i:i+16])
            f.write(f"    {chunk},\n")
        f.write("};\n\n")
        f.write("#endif /* IXLOGO_ANIM_H */\n")

    print(f"Successfully generated {out_h} ({os.path.getsize(out_h):,} bytes)")

if __name__ == "__main__":
    generate()
