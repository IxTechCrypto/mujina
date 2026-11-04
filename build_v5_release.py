import os
import shutil
import subprocess
import struct
import zlib
import hashlib

def run_cmd(cmd, cwd=None):
    print(f"Running: {cmd}")
    res = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    if res.returncode != 0:
        print(f"STDOUT: {res.stdout}")
        print(f"STDERR: {res.stderr}")
        raise RuntimeError(f"Command failed: {cmd}")
    return res.stdout

print("[1] Updating staged mujina-minerd binary...")
src_bin = "/home/grant/nano3s-build/nano-mujina/target/riscv64gc-unknown-linux-gnu/release/mujina-minerd.stripped"
dst_bin = "/home/grant/udisk_build/data_extracted/590221451/ubi_data_part/mujina-minerd"
shutil.copy2(src_bin, dst_bin)
os.chmod(dst_bin, 0o755)

print("[2] Creating clean staging tree for genimage...")
stage_dir = "/home/grant/udisk_build/stage_v5"
data_stage = os.path.join(stage_dir, "data")
if os.path.exists(stage_dir):
    shutil.rmtree(stage_dir)
os.makedirs(data_stage, exist_ok=True)
run_cmd(f"cp -a /home/grant/udisk_build/data_extracted/590221451/ubi_data_part/* {data_stage}/")

# Clean up any transient logs/caches
for root, dirs, files in os.walk(data_stage):
    for f in files:
        if f.endswith('.log') or f.endswith('.tmp') or f.startswith('.'):
            try:
                os.remove(os.path.join(root, f))
            except Exception:
                pass

# Clean userconfig wpa_supplicant.conf
wpa_path = os.path.join(data_stage, "userconfig", "wpa_supplicant.conf")
with open(wpa_path, "w") as f:
    f.write("ctrl_interface=/var/run/wpa_supplicant\nupdate_config=1\n")

# Data-only genimage config
cfg_path = "/home/grant/udisk_build/genimage-data-only.cfg"
with open(cfg_path, "w") as f:
    f.write("""flash spinand-256M {
	pebsize = 131072
	lebsize = 126976
	numpebs = 2048
	minimum-io-unit-size = 2048
	vid-header-offset = 2048
	sub-page-size = 2048
}

image data.ubifs {
	ubifs {
		extraargs = "-x zlib -U"
		space-fixup = true
	}
	size = 51298304
	mountpoint = "data"
	flashtype = "spinand-256M"
}

image data.ubi {
	ubi {
		extraargs = ""
		minimize = true
	}
	partition ubi_data_part {
		image = "data.ubifs"
		autoresize = true
	}
	size = 56M
	flashtype = "spinand-256M"
}
""")

print("[3] Running genimage to generate UBI data partition...")
out_v5 = "/home/grant/udisk_build/out_v5"
gtmp_v5 = "/home/grant/udisk_build/gtmp_v5"
if os.path.exists(out_v5):
    shutil.rmtree(out_v5)
if os.path.exists(gtmp_v5):
    shutil.rmtree(gtmp_v5)
os.makedirs(out_v5, exist_ok=True)
os.makedirs(gtmp_v5, exist_ok=True)

genimage_bin = "/home/grant/udisk_build/build/tools/genimage"
run_cmd(f"{genimage_bin} --rootpath {stage_dir} --tmppath {gtmp_v5} --inputpath {out_v5} --outputpath {out_v5} --config {cfg_path}")

raw_data_ubi = os.path.join(out_v5, "data.ubi")
raw_data = open(raw_data_ubi, "rb").read()
print(f"genimage produced data.ubi: {len(raw_data)} bytes")

# Read empty PEB template from v4_data.img
with open("/home/grant/udisk_build/v4_data.img", "rb") as f:
    f.seek(120 * 131072)
    empty_peb = f.read(131072)

pebs_needed = 448 - (len(raw_data) // 131072)
print(f"Padding {pebs_needed} empty PEBs to reach 448 PEBs (58,720,256 bytes)...")
full_data = raw_data + empty_peb * pebs_needed
assert len(full_data) == 58720256, f"Full data size mismatch: {len(full_data)}"

v5_data_img = "/home/grant/udisk_build/v5_data.img"
with open(v5_data_img, "wb") as f:
    f.write(full_data)
print(f"Wrote {v5_data_img} ({len(full_data)} bytes)")

print("[4] Repacking nano-mujina-v5-release.kdimg...")
src_kdimg = "/home/grant/udisk_build/nano-mujina-v4.kdimg"
out_kdimg = "/home/grant/udisk_build/nano-mujina-v5-release.kdimg"
pack_py = "/mnt/d/github/torii/tools/nano3s-image/pack_kdimg.py"
run_cmd(f"python3 {pack_py} {src_kdimg} {out_kdimg} data={v5_data_img}")

kdimg_size = os.path.getsize(out_kdimg)
print(f"Generated kdimg size: {kdimg_size} bytes (expected 129,232,128)")
assert kdimg_size == 129232128, f"kdimg size mismatch: {kdimg_size} != 129232128"

print("[5] Verifying partition table and CRC32...")
raw = open(out_kdimg, "rb").read()
magic, header_crc, _, version, count, tbl_crc = struct.unpack_from("<6I", raw, 0)
assert magic == 0x27cb8f93, f"Bad magic: {magic:#x}"
tbl = raw[512:512 + count * 256]
calc_tbl_crc = zlib.crc32(tbl) & 0xFFFFFFFF
assert tbl_crc == calc_tbl_crc, f"Table CRC mismatch: {tbl_crc:#x} != {calc_tbl_crc:#x}"

zeroed_header = bytearray(raw[:512])
struct.pack_into("<I", zeroed_header, 4, 0)
calc_hdr_crc = zlib.crc32(zeroed_header) & 0xFFFFFFFF
assert header_crc == calc_hdr_crc, f"Header CRC mismatch: {header_crc:#x} != {calc_hdr_crc:#x}"
print(f"CRC checks passed: header_crc={header_crc:#x}, tbl_crc={tbl_crc:#x}")

print("[6] Copying release artifacts to Windows Downloads...")
win_downloads = "/mnt/c/Users/grant/Downloads"
shutil.copy2(out_kdimg, os.path.join(win_downloads, "nano-mujina-v5-release.kdimg"))
shutil.copy2(v5_data_img, os.path.join(win_downloads, "v5_data_release.img"))
print("[+] Build complete! Both nano-mujina-v5-release.kdimg and v5_data_release.img ready in C:\\Users\\grant\\Downloads.")
