"""Apply the power-on splash when packaging the Nano3S USB release."""

import gzip
import hashlib
from pathlib import Path
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
import zlib


SPLASH = Path(__file__).with_name("assets") / "nano3s-boot-splash.png"
SCREEN_SIZE = (240, 240)
# The bitmap offset is specific to the stock U-Boot in the v5 release base.
SPLASH_OFFSET = 447056
STOCK_UBOOT_SHA256 = "86466edd1e7ac9c931fd0176d7ee286ce35bafce65e057d7797d7accbce9ff1a"
K230_HEADER_SIZE = 528
UIMAGE_HEADER_SIZE = 64
NAND_PAGE_SIZE = 2048
NAND_ERASE_SIZE = 128 * 1024
NAND_SIZE = 256 * 1024 * 1024
UBOOT_SLOTS = {"uboot_a": 0x100000, "uboot_b": 0x300000}
UBOOT_SLOT_SIZE = 0x200000
KDIMG_HEADER_SIZE = 512
KDIMG_ENTRY_SIZE = 256


def splash_pixels():
    from PIL import Image

    with Image.open(SPLASH) as image:
        if image.size != SCREEN_SIZE or image.mode != "RGB":
            raise ValueError("The Nano3S splash must be a 240x240 RGB image")
        channels = iter(image.tobytes())
        return b"".join(
            struct.pack(">H", (red >> 3) << 11 | (green >> 2) << 5 | (blue >> 3))
            for red, green, blue in zip(channels, channels, channels)
        )


def header_crc(header):
    return zlib.crc32(header[:4] + bytes(4) + header[8:])


def unpack_uboot(wrapped):
    magic, size, mode = struct.unpack_from("<4sII", wrapped)
    payload = wrapped[K230_HEADER_SIZE:K230_HEADER_SIZE + size]
    if magic != b"K230" or mode != 0 or len(payload) != size:
        raise ValueError("Unexpected K230 U-Boot wrapper")
    if hashlib.sha256(payload).digest() != wrapped[12:44]:
        raise ValueError("Invalid K230 U-Boot payload hash")

    version = payload[:4]
    header = payload[4:4 + UIMAGE_HEADER_SIZE]
    image = payload[4 + UIMAGE_HEADER_SIZE:]
    magic, crc, _, length, _, _, image_crc = struct.unpack_from(">7I", header)
    if magic != 0x27051956 or crc != header_crc(header) or length != len(image):
        raise ValueError("Invalid U-Boot uImage header")
    if zlib.crc32(image) != image_crc or image[:4] != b"\x1f\x8b\x09\x00":
        raise ValueError("Invalid U-Boot compressed image")
    # K230 uses gzip's method byte 0x09 for hardware decompression.
    return version, header, image[:10], gzip.decompress(image[:2] + b"\x08" + image[3:])


def patch_uboot(wrapped, pixels, slot):
    if hashlib.sha256(wrapped).hexdigest() != STOCK_UBOOT_SHA256:
        raise ValueError(f"{slot}: unsupported Nano3S base U-Boot payload")

    version, header, gzip_header, original = unpack_uboot(wrapped)
    end = SPLASH_OFFSET + len(pixels)
    if len(pixels) != SCREEN_SIZE[0] * SCREEN_SIZE[1] * 2 or end > len(original):
        raise ValueError("Invalid Nano3S splash bitmap size")
    plain = original[:SPLASH_OFFSET] + pixels + original[end:]
    compressor = zlib.compressobj(level=9, wbits=-15)
    stream = compressor.compress(plain) + compressor.flush()
    image = gzip_header + stream + struct.pack("<II", zlib.crc32(plain), len(plain))

    legacy = bytearray(header)
    struct.pack_into(">I", legacy, 12, len(image))
    struct.pack_into(">I", legacy, 24, zlib.crc32(image))
    struct.pack_into(">I", legacy, 4, header_crc(legacy))
    payload = version + legacy + image
    k230 = bytearray(wrapped[:K230_HEADER_SIZE])
    struct.pack_into("<I", k230, 4, len(payload))
    k230[12:44] = hashlib.sha256(payload).digest()

    result = bytes(k230) + payload
    result += b"\xff" * (-len(result) % NAND_PAGE_SIZE)
    if len(result) > UBOOT_SLOT_SIZE or unpack_uboot(result)[3] != plain:
        raise ValueError(f"{slot}: rebuilt U-Boot failed validation")
    return result


def verify_release_image(path, replacements):
    image_size = path.stat().st_size
    with path.open("rb") as file:
        header = file.read(KDIMG_HEADER_SIZE)
        magic, crc, _, version, count, table_crc = struct.unpack_from("<6I", header)
        if magic != 0x27CB8F93 or version != 1 or crc != header_crc(header):
            raise ValueError("Invalid Nano3S .kdimg header")
        if not 0 < count <= (image_size - KDIMG_HEADER_SIZE) // KDIMG_ENTRY_SIZE:
            raise ValueError("Invalid .kdimg partition count")
        table = file.read(count * KDIMG_ENTRY_SIZE)
        if zlib.crc32(table) != table_crc:
            raise ValueError("Invalid .kdimg partition table checksum")

        content_end = file.tell()
        flash_end = 0
        names = set()
        for index in range(count):
            entry = table[index * KDIMG_ENTRY_SIZE:(index + 1) * KDIMG_ENTRY_SIZE]
            tag, dest, write, erase, limit, flags, offset, length = struct.unpack_from("<8I", entry)
            name = entry[64:96].split(b"\0", 1)[0].decode("ascii")
            if tag != 0x91DF6DA4 or name in names or flags != 0:
                raise ValueError(f"{name}: invalid .kdimg entry")
            names.add(name)
            if offset != content_end or not 0 < length <= write or offset + length > image_size:
                raise ValueError(f"{name}: invalid .kdimg content range")
            write_span = write + (-write % NAND_ERASE_SIZE)
            span = max(write_span, erase)
            if dest % NAND_ERASE_SIZE or erase % NAND_ERASE_SIZE or dest < flash_end or dest + span > NAND_SIZE:
                raise ValueError(f"{name}: invalid Nano3S flash range")
            if limit and write > limit:
                raise ValueError(f"{name}: image exceeds partition size")
            if name in UBOOT_SLOTS:
                if dest != UBOOT_SLOTS[name] or limit != UBOOT_SLOT_SIZE or span > UBOOT_SLOT_SIZE:
                    raise ValueError(f"{name}: unexpected U-Boot slot layout")

            file.seek(offset)
            digest = hashlib.sha256(file.read(length)).digest()
            if digest != entry[32:64]:
                raise ValueError(f"{name}: invalid .kdimg payload hash")
            if name in replacements and digest != replacements[name]:
                raise ValueError(f"{name}: .kdimg payload differs from release input")
            content_end = offset + length
            flash_end = dest + span

    if content_end != image_size or not replacements.keys() <= names:
        raise ValueError("Incomplete Nano3S release image")


def pack_release_image(packer, source, destination, app_image, data_image):
    pixels = splash_pixels()
    destination = Path(destination)
    with TemporaryDirectory(dir=destination.parent) as directory:
        stage = Path(directory)
        subprocess.run(
            [sys.executable, packer, "--extract", source, stage, *UBOOT_SLOTS], check=True
        )
        replacements = {"app_ubi": Path(app_image), "data": Path(data_image)}
        for slot in UBOOT_SLOTS:
            path = stage / f"{slot}.bin"
            path.write_bytes(patch_uboot(path.read_bytes(), pixels, slot))
            replacements[slot] = path

        image = stage / destination.name
        arguments = [f"{name}={path}" for name, path in replacements.items()]
        subprocess.run([sys.executable, packer, source, image, *arguments], check=True)
        hashes = {
            name: hashlib.sha256(path.read_bytes()).digest()
            for name, path in replacements.items()
        }
        verify_release_image(image, hashes)
        image.replace(destination)
