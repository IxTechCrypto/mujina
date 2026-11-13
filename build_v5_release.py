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

print("=== MUJINA FIRMWARE RELEASE BUILD (v5.0.8) ===")
print("[1] Updating staged binaries for data and app partitions...")

# Data binaries
src_minerd = "/home/grant/nano3s-build/nano-mujina/target/riscv64gc-unknown-linux-gnu/release/mujina-minerd.stripped"
dst_minerd = "/home/grant/udisk_build/data_extracted/590221451/ubi_data_part/mujina-minerd"
shutil.copy2(src_minerd, dst_minerd)
os.chmod(dst_minerd, 0o755)

src_ble = "/home/grant/nano3s-build/nano-mujina/target/riscv64gc-unknown-linux-gnu/release/ble_setup.stripped"
dst_ble = "/home/grant/udisk_build/data_extracted/590221451/ubi_data_part/ble_setup"
shutil.copy2(src_ble, dst_ble)
os.chmod(dst_ble, 0o755)

src_ui = "/home/grant/nano3s-build/nano-mujina/nano3s_ui/target/riscv64gc-unknown-linux-gnu/release/nano3s_ui"
dst_ui = "/home/grant/udisk_build/data_extracted/590221451/ubi_data_part/nano3s_ui"
shutil.copy2(src_ui, dst_ui)
os.chmod(dst_ui, 0o755)

# App binaries (Core 1 RTOS + Display startup watchdog + Button handler)
src_rtos = "/home/grant/nano3s-build/nano-mujina/rtos_core/build/rtos_core.elf"
dst_rtos = "/home/grant/udisk_build/rootfs_extracted/248013053/ubi_app_part/release/rtt/rtos_core.elf"
shutil.copy2(src_rtos, dst_rtos)
os.chmod(dst_rtos, 0o755)

src_startup = "/home/grant/nano3s-build/nano-mujina/deploy/mujina_display_startup.sh"
dst_startup = "/home/grant/udisk_build/rootfs_extracted/248013053/ubi_app_part/release/linux/app/mujina_display_startup.sh"
shutil.copy2(src_startup, dst_startup)
os.chmod(dst_startup, 0o755)

src_button = "/home/grant/nano3s-build/nano-mujina/fb_hijack/target/riscv64gc-unknown-linux-gnu/release/fb_button"
dst_button = "/home/grant/udisk_build/rootfs_extracted/248013053/ubi_app_part/release/linux/app/fb_button"
shutil.copy2(src_button, dst_button)
os.chmod(dst_button, 0o755)

print(f"  rtos_core.elf: {hashlib.sha256(open(dst_rtos, 'rb').read()).hexdigest()}")
print(f"  mujina-minerd: {hashlib.sha256(open(dst_minerd, 'rb').read()).hexdigest()}")
print(f"  nano3s_ui:     {hashlib.sha256(open(dst_ui, 'rb').read()).hexdigest()}")
print(f"  fb_button:     {hashlib.sha256(open(dst_button, 'rb').read()).hexdigest()}")

print("[2] Creating clean staging tree for app and data...")
stage_dir = "/home/grant/udisk_build/stage_v5"
data_stage = os.path.join(stage_dir, "data")
app_stage = os.path.join(stage_dir, "app")
out_v5 = "/home/grant/udisk_build/out_v5"
gtmp_v5 = "/home/grant/udisk_build/gtmp_v5"

if os.path.exists(stage_dir):
    shutil.rmtree(stage_dir)
if os.path.exists(out_v5):
    shutil.rmtree(out_v5)
if os.path.exists(gtmp_v5):
    shutil.rmtree(gtmp_v5)

os.makedirs(data_stage, exist_ok=True)
os.makedirs(app_stage, exist_ok=True)
os.makedirs(out_v5, exist_ok=True)
os.makedirs(gtmp_v5, exist_ok=True)

# Stage Data
run_cmd(f"cp -a /home/grant/udisk_build/data_extracted/590221451/ubi_data_part/* {data_stage}/")
for root, dirs, files in os.walk(data_stage):
    for f in files:
        if (f.endswith('.log') or '.log.' in f or f.endswith('.tmp')
                or f.endswith('.real_backup') or f.startswith('.')):
            try:
                os.remove(os.path.join(root, f))
            except Exception:
                pass

# [P1] Strip unit-specific donor factory files and hardware serials
donor_files = [
    os.path.join(data_stage, "factory", "hashrate_cali.ini"),
    os.path.join(data_stage, "factory", "efuse_info.txt"),
    os.path.join(data_stage, "userdata", "nano", "board_id_cache"),
    os.path.join(data_stage, "userconfig", "activation.ini"),
]
for df in donor_files:
    if os.path.exists(df):
        os.remove(df)
        print(f"  [P1] Stripped donor artifact: {os.path.basename(df)}")

# [P1] Sanitize pool configs to clean default placeholder (replacing donor wallet)
pool_path = os.path.join(data_stage, "userconfig", "pool.conf")
with open(pool_path, "w") as f:
    f.write("url=stratum+tcp://pool.256foundation.org:3333\nuser=YOUR_WALLET.YOUR_WORKER\n")

cgminer_path = os.path.join(data_stage, "userconfig", "cgminer.conf")
with open(cgminer_path, "w") as f:
    f.write("""{\n  "pools": {\n    "pool0": {\n      "url": "stratum+tcp://pool.256foundation.org:3333",\n      "user": "YOUR_WALLET.YOUR_WORKER",\n      "pass": "x"\n    }\n  },\n  "standard": "--lowmem --real-quiet"\n}\n""")

wpa_path = os.path.join(data_stage, "userconfig", "wpa_supplicant.conf")
with open(wpa_path, "w") as f:
    f.write("ctrl_interface=/var/run/wpa_supplicant\nupdate_config=1\n")

# Stage App
run_cmd(f"cp -a /home/grant/udisk_build/rootfs_extracted/248013053/ubi_app_part/* {app_stage}/")
for root, dirs, files in os.walk(app_stage):
    for f in files:
        if (f.endswith('.log') or '.log.' in f or f.endswith('.tmp')
                or f.endswith('.real_backup') or f.startswith('.')
                or f == 'biglog' or '.pre_' in f or '.stock_backup' in f):
            try:
                os.remove(os.path.join(root, f))
            except Exception:
                pass

# [P0] Fix execute permissions across app partition before mkfs.ubifs
app_bin_dir = os.path.join(app_stage, "release", "linux", "app")
scripts_750 = ["volt_bringup.sh", "gpio34_high_bringup.sh", "backlight_bringup.sh",
               "wifi_bringup.sh", "fan_bringup.sh"]
for s in scripts_750:
    p = os.path.join(app_bin_dir, s)
    if os.path.exists(p):
        os.chmod(p, 0o750)

bins_755 = ["wdt_disable_hold", "fb_button", "fb_draw", "mujina_test_harness", "mujina_display_startup.sh"]
for b in bins_755:
    p = os.path.join(app_bin_dir, b)
    if os.path.exists(p):
        os.chmod(p, 0o755)

rtos_elf = os.path.join(app_stage, "release", "rtt", "rtos_core.elf")
if os.path.exists(rtos_elf):
    os.chmod(rtos_elf, 0o755)

# [P0 Assert] Verify execute bits are strictly enforced
for s in scripts_750:
    p = os.path.join(app_bin_dir, s)
    mode = oct(os.stat(p).st_mode & 0o777)
    assert mode in ("0o750", "0o755"), f"Permission failure on {s}: mode is {mode}"
for b in bins_755:
    p = os.path.join(app_bin_dir, b)
    mode = oct(os.stat(p).st_mode & 0o777)
    assert mode == "0o755", f"Permission failure on {b}: mode is {mode}"
print("  [P0] Asserted execution permissions (0o750 / 0o755) across all app binaries and scripts.")

print("[3] Building app UBI partition (v5_app.img)...")
app_ubifs = os.path.join(out_v5, "app.ubifs")
run_cmd(f"/usr/sbin/mkfs.ubifs -r {app_stage} -m 2048 -e 126976 -c 83 -x zlib -U -F -o {app_ubifs}")

ubinize_app_ini = "/home/grant/udisk_build/ubinize_app_v5.ini"
raw_app_ubi = os.path.join(out_v5, "app.ubi")
with open(ubinize_app_ini, "w") as f:
    f.write(f"""[ubi_app_part]
mode=ubi
image={app_ubifs}
vol_id=0
vol_type=dynamic
vol_name=ubi_app_part
vol_size=10539008
""")
run_cmd(f"/usr/sbin/ubinize -o {raw_app_ubi} -p 131072 -m 2048 -s 2048 -Q 248013053 {ubinize_app_ini}")

raw_app = open(raw_app_ubi, "rb").read()
print(f"ubinize produced app.ubi: {len(raw_app)} bytes ({len(raw_app) // 131072} PEBs)")

# Empty PEB template for app_ubi from partition 9 of nano-mujina-v4.kdimg (PEB 0, image_seq=248013053)
with open("/home/grant/udisk_build/nano-mujina-v4.kdimg", "rb") as f:
    f.seek(0x0373ed00)
    empty_app_peb = f.read(131072)

app_pebs_needed = 96 - (len(raw_app) // 131072)
print(f"Padding {app_pebs_needed} empty PEBs to reach 96 PEBs (12,582,912 bytes)...")
full_app = raw_app + empty_app_peb * app_pebs_needed
assert len(full_app) == 12582912, f"Full app size mismatch: {len(full_app)}"

# Verify all 96 PEBs of app partition
expected_app_seq = 248013053
app_vol_table_pebs = []
for peb_idx in range(96):
    peb_off = peb_idx * 131072
    ec_magic = full_app[peb_off:peb_off + 4]
    assert ec_magic == b"UBI#", f"App PEB {peb_idx} missing UBI# magic"
    seq = struct.unpack_from(">I", full_app, peb_off + 24)[0]
    assert seq == expected_app_seq, f"App PEB {peb_idx} image_seq mismatch: {seq:#x} != {expected_app_seq:#x}"
    vid_magic = full_app[peb_off + 2048:peb_off + 2052]
    if vid_magic == b"UBI!":
        vol_id = struct.unpack_from(">I", full_app, peb_off + 2048 + 8)[0]
        if vol_id == 0x7fffefff:
            app_vol_table_pebs.append(peb_idx)
            reserved_pebs = struct.unpack_from(">I", full_app, peb_off + 4096)[0]
            assert reserved_pebs == 83, f"Unexpected reserved_pebs={reserved_pebs} in app PEB {peb_idx}"

assert len(app_vol_table_pebs) == 2, f"Expected 2 layout PEBs in app, found {app_vol_table_pebs}"
print(f"Verified all 96 PEBs of app have image_seq={expected_app_seq:#x}, layout PEBs={app_vol_table_pebs}, reserved_pebs=83")

v5_app_img = "/home/grant/udisk_build/v5_app.img"
with open(v5_app_img, "wb") as f:
    f.write(full_app)
print(f"Wrote {v5_app_img} ({len(full_app)} bytes)")

print("[4] Building data UBI partition (v5_data.img)...")
cfg_path = "/home/grant/udisk_build/genimage-data-only.cfg"
genimage_bin = "/home/grant/udisk_build/build/tools/genimage"
run_cmd(f"{genimage_bin} --rootpath {stage_dir} --tmppath {gtmp_v5} --inputpath {out_v5} --outputpath {out_v5} --config {cfg_path}")

ubinize_ini = "/home/grant/udisk_build/ubinize_v5.ini"
data_ubifs = os.path.join(out_v5, "data.ubifs")
raw_data_ubi = os.path.join(out_v5, "data.ubi")
with open(ubinize_ini, "w") as f:
    f.write(f"""[ubi_data_part]
mode=ubi
image={data_ubifs}
vol_id=0
vol_type=dynamic
vol_name=ubi_data_part
vol_flags=autoresize
""")
run_cmd(f"/usr/sbin/ubinize -o {raw_data_ubi} -p 131072 -m 2048 -s 2048 -Q 590221451 {ubinize_ini}")

raw_data = open(raw_data_ubi, "rb").read()
print(f"ubinize produced data.ubi: {len(raw_data)} bytes ({len(raw_data) // 131072} PEBs)")

with open("/home/grant/udisk_build/v4_data.img", "rb") as f:
    f.seek(120 * 131072)
    empty_peb = f.read(131072)

pebs_needed = 448 - (len(raw_data) // 131072)
print(f"Padding {pebs_needed} empty PEBs to reach 448 PEBs (58,720,256 bytes)...")
full_data = raw_data + empty_peb * pebs_needed
assert len(full_data) == 58720256, f"Full data size mismatch: {len(full_data)}"

expected_seq = 590221451
vol_table_pebs = []
for peb_idx in range(448):
    peb_off = peb_idx * 131072
    ec_magic = full_data[peb_off:peb_off + 4]
    assert ec_magic == b"UBI#", f"PEB {peb_idx} missing UBI# magic"
    seq = struct.unpack_from(">I", full_data, peb_off + 24)[0]
    assert seq == expected_seq, f"PEB {peb_idx} image_seq mismatch: {seq:#x} != {expected_seq:#x}"
    vid_magic = full_data[peb_off + 2048:peb_off + 2052]
    if vid_magic == b"UBI!":
        vol_id = struct.unpack_from(">I", full_data, peb_off + 2048 + 8)[0]
        if vol_id == 0x7fffefff:
            vol_table_pebs.append(peb_idx)
            reserved_pebs = struct.unpack_from(">I", full_data, peb_off + 4096)[0]
            vol_flags = full_data[peb_off + 4096 + 144]
            assert 100 <= reserved_pebs <= 140, f"Unexpected reserved_pebs={reserved_pebs} in PEB {peb_idx}"
            assert vol_flags == 1, f"Expected autoresize flag (1), got {vol_flags} in PEB {peb_idx}"

assert len(vol_table_pebs) == 2, f"Expected 2 layout volume PEBs, found {vol_table_pebs}"
print(f"Verified all 448 PEBs have image_seq={expected_seq:#x}, layout PEBs={vol_table_pebs}, reserved_pebs={reserved_pebs}, vol_flags={vol_flags}")

v5_data_img = "/home/grant/udisk_build/v5_data.img"
with open(v5_data_img, "wb") as f:
    f.write(full_data)
print(f"Wrote {v5_data_img} ({len(full_data)} bytes)")

print("[4.5] Generating OTA bundle (nano-mujina-v5-bundle.bin)...")
minerd_bytes = open(dst_minerd, "rb").read()
bundle_magic = b"MJBUNDL1"
harness_bytes = b""  # Omitted in this bundle (length = 0)
bundle_data = (
    bundle_magic
    + struct.pack("<I", len(minerd_bytes))
    + minerd_bytes
    + struct.pack("<I", len(harness_bytes))
    + harness_bytes
)
bundle_bin = os.path.join(out_v5, "nano-mujina-v5-bundle.bin")
with open(bundle_bin, "wb") as f:
    f.write(bundle_data)
bundle_size = os.path.getsize(bundle_bin)
bundle_sha256 = hashlib.sha256(bundle_data).hexdigest()
print(f"  Built OTA bundle: {bundle_bin} ({bundle_size} bytes, SHA-256: {bundle_sha256})")

print("[5] Repacking nano-mujina-v5-release.kdimg with updated app_ubi and data...")
src_kdimg = "/home/grant/udisk_build/nano-mujina-v4.kdimg"
out_kdimg = "/home/grant/udisk_build/nano-mujina-v5-release.kdimg"
pack_py = "/mnt/d/github/torii/tools/nano3s-image/pack_kdimg.py"
run_cmd(f"python3 {pack_py} {src_kdimg} {out_kdimg} app_ubi={v5_app_img} data={v5_data_img}")

kdimg_size = os.path.getsize(out_kdimg)
print(f"Generated kdimg size: {kdimg_size} bytes (expected 129,232,128)")
assert kdimg_size == 129232128, f"kdimg size mismatch: {kdimg_size} != 129232128"

print("[6] Verifying partition table and CRC32...")
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

print("[7] Copying release artifacts to Windows Downloads...")
kdimg_sha256 = hashlib.sha256(raw).hexdigest()
app_sha256 = hashlib.sha256(full_app).hexdigest()
data_sha256 = hashlib.sha256(full_data).hexdigest()
print(f"kdimg SHA-256: {kdimg_sha256}")
print(f"app   SHA-256: {app_sha256}")
print(f"data  SHA-256: {data_sha256}")

VERSION = "v5.0.8"

for dl_dir in ["/mnt/c/Users/grant/Downloads", "/mnt/d/downloads"]:
    if os.path.isdir(dl_dir):
        # Version-explicit filenames
        shutil.copy2(out_kdimg, os.path.join(dl_dir, f"nano-mujina-{VERSION}-release.kdimg"))
        shutil.copy2(v5_app_img, os.path.join(dl_dir, f"{VERSION}_app_release.img"))
        shutil.copy2(v5_data_img, os.path.join(dl_dir, f"{VERSION}_data_release.img"))
        shutil.copy2(bundle_bin, os.path.join(dl_dir, f"nano-mujina-{VERSION}-bundle.bin"))
        # Unversioned aliases for backward compatibility with tooling
        shutil.copy2(out_kdimg, os.path.join(dl_dir, "nano-mujina-v5-release.kdimg"))
        shutil.copy2(v5_app_img, os.path.join(dl_dir, "v5_app_release.img"))
        shutil.copy2(v5_data_img, os.path.join(dl_dir, "v5_data_release.img"))
        shutil.copy2(bundle_bin, os.path.join(dl_dir, "nano-mujina-v5-bundle.bin"))
        print(f"Copied artifacts to {dl_dir} (with {VERSION} and v5 naming)")
        dup_path = os.path.join(dl_dir, "nano-mujina-v5-release (1).kdimg")
        if os.path.exists(dup_path):
            shutil.copy2(out_kdimg, dup_path)
            print(f"Updated existing {dup_path}")

# Torii workspace sync
torii_dir = "/mnt/d/github/torii"
if os.path.isdir(torii_dir):
    shutil.copy2(bundle_bin, os.path.join(torii_dir, "bundle.bin"))
    shutil.copy2(bundle_bin, os.path.join(torii_dir, f"nano-mujina-{VERSION}-bundle.bin"))
    print(f"Copied bundle to {torii_dir}")

print("[+] Build complete successfully!")

