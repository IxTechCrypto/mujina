import os
import glob
import zlib
from PIL import Image
import numpy as np

def generate():
    frames_dir = r"C:\Users\grant\.gemini\antigravity\brain\fa08a51d-7d2a-471f-accf-2797da253ec2\scratch\frames_all"
    files = sorted(glob.glob(os.path.join(frames_dir, "frame_*.png")))
    if len(files) < 300:
        raise ValueError(f"Expected 300 frames, found {len(files)}")
    
    # 15 fps: sample every 2 frames -> 150 frames
    sample_files = files[::2]
    num_frames = len(sample_files)
    print(f"Sampling {num_frames} frames (15 fps, 10s)")

    # Crop to 114x114 centered at (62, 63) to (176, 177)
    crops = []
    for f in sample_files:
        im = Image.open(f).crop((62, 63, 176, 177))
        crops.append(np.array(im, dtype=np.float32))

    # Apply seamless cross-fade on last 10 frames towards frame 0
    f0 = crops[0]
    fade_len = 10
    for idx in range(fade_len):
        frame_idx = num_frames - fade_len + idx
        alpha = (idx + 1) / (fade_len + 1)
        crops[frame_idx] = (1.0 - alpha) * crops[frame_idx] + alpha * f0

    # Build composite image for global quantization
    # 15 columns x 10 rows
    comp_w = 114 * 15
    comp_h = 114 * 10
    comp = Image.new("RGB", (comp_w, comp_h))
    for i, arr in enumerate(crops):
        im = Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))
        x = (i % 15) * 114
        y = (i // 15) * 114
        comp.paste(im, (x, y))

    # Quantize to 32 colors
    num_colors = 32
    pal_img = comp.quantize(colors=num_colors, method=Image.Quantize.FASTOCTREE)
    raw_pal = pal_img.getpalette()[:num_colors * 3]

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

    # Ensure index of pure black or darkest color
    print(f"Palette generated with {num_colors} colors")

    # Quantize each frame
    raw_payload = bytearray()
    for arr in crops:
        im = Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))
        q = im.quantize(palette=pal_img)
        raw_payload.extend(q.tobytes())

    uncompressed_len = len(raw_payload)
    print(f"Uncompressed payload: {uncompressed_len:,} bytes ({uncompressed_len/1024:.1f} KB)")

    # Raw DEFLATE compression (wbits = -15 for raw DEFLATE without zlib header/trailer)
    comp_obj = zlib.compressobj(level=9, method=zlib.DEFLATED, wbits=-15)
    deflate_bytes = comp_obj.compress(bytes(raw_payload)) + comp_obj.flush()
    print(f"Raw DEFLATE compressed size: {len(deflate_bytes):,} bytes ({len(deflate_bytes)/1024:.1f} KB)")

    # Verify decompression with zlib
    decomp = zlib.decompress(deflate_bytes, -15)
    assert decomp == bytes(raw_payload), "Decompression self-check failed!"
    print("Self-check passed: DEFLATE decompress matches raw payload exactly!")

    # Write C header
    out_h = r"D:\github\mujina-v5.2-staging\nano3s_ui\c_driver\ixlogo_anim.h"
    with open(out_h, "w", encoding="utf-8") as f:
        f.write("/* Auto-generated IxTech animated logo assets (15 fps, 10.0s, 32 colors) */\n")
        f.write("#ifndef IXLOGO_ANIM_H\n")
        f.write("#define IXLOGO_ANIM_H\n\n")
        f.write("#include <stdint.h>\n\n")
        f.write(f"#define IXLOGO_FRAME_W       114\n")
        f.write(f"#define IXLOGO_FRAME_H       114\n")
        f.write(f"#define IXLOGO_NUM_FRAMES    {num_frames}\n")
        f.write(f"#define IXLOGO_FRAME_BYTES   (IXLOGO_FRAME_W * IXLOGO_FRAME_H)\n")
        f.write(f"#define IXLOGO_TOTAL_BYTES   {uncompressed_len}\n")
        f.write(f"#define IXLOGO_FRAME_MS      66 /* 15 FPS */\n\n")

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
