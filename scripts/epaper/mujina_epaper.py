#!/usr/bin/env python3
import os
import sys
import time
import socket
import requests
from PIL import Image, ImageDraw, ImageFont

# --- CONFIGURATION ---
# Set this to your exact Waveshare e-paper model module name. Examples:
# epd2in13_V4, epd2in9, epd2in7, epd4in2, etc.
EPD_MODEL = "epd2in13_V4"
API_URL = "http://127.0.0.1:7785/api/v0/miner"
REFRESH_INTERVAL = 60 # seconds

# Helper to find our active local network IP address
def get_ip_address():
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
        return "127.0.0.1"

# Dynamically import the Waveshare EPD library matching EPD_MODEL
try:
    epd_module = __import__(f"waveshare_epd.{EPD_MODEL}", fromlist=[EPD_MODEL])
except ImportError:
    print(f"Error: Could not import waveshare_epd.{EPD_MODEL}")
    print("Please make sure waveshare-epd is installed or the waveshare_epd folder is present.")
    sys.exit(1)

def main():
    print(f"Initializing Waveshare EPD: {EPD_MODEL}")
    epd = epd_module.EPD()
    epd.init()
    epd.Clear(0xFF) # Clear screen to white (0xFF)

    # Load standard DejaVu Sans fonts available on Raspberry Pi OS
    try:
        font_lg = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 26)
        font_title = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 11)
        font_body = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 10)
        font_sm = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 9)
    except IOError:
        font_lg = font_title = font_body = font_sm = ImageFont.load_default()

    # Get dimensions (waveshare drivers automatically swap width/height if landscape-oriented)
    width = epd.width
    height = epd.height
    print(f"Display dimensions: {width}x{height}")

    while True:
        # Create monochrome e-paper canvas image buffer (255 = White, 0 = Black)
        image = Image.new("1", (width, height), 255)
        draw = ImageDraw.Draw(image)

        ip_addr = get_ip_address()
        
        # Default telemetry values
        status_text = "OFFLINE"
        hashrate = "0.00 TH/s"
        boards_online = 0
        uptime = "0h 0m"
        pool = "N/A"

        try:
            r = requests.get(API_URL, timeout=4)
            if r.status_code == 200:
                data = r.json()
                status_text = "ONLINE"
                
                # Convert raw H/s hashrate to TH/s (1 TH/s = 1e12 hashes/sec)
                raw_hashrate = data.get("hashrate", 0)
                hashrate = f"{raw_hashrate / 1000000000000.0:.2f} TH/s"
                
                # Count boards online from the active boards list
                boards_online = len(data.get("boards", []))
                
                # Convert uptime_secs to a readable format
                uptime_secs = data.get("uptime_secs", 0)
                hours = uptime_secs // 3600
                minutes = (uptime_secs % 3600) // 60
                uptime = f"{hours}h {minutes}m"
                
                # Get the active pool URL from the first job source
                sources = data.get("sources", [])
                if sources:
                    pool = sources[0].get("url", "N/A")
                    if pool.startswith("stratum+tcp://"):
                        pool = pool[len("stratum+tcp://"):]
                else:
                    pool = "N/A"
        except Exception:
            status_text = "OFFLINE"

        # Separate hashrate value and unit
        val_str = hashrate.split()[0]
        unit_str = "TH/s"

        # Truncate pool name for layout fit
        if len(pool) > 16:
            pool = pool[:14] + ".."

        # Adaptive layout rendering depending on display aspect ratio orientation
        is_landscape = width > height
        if is_landscape:
            # Cyberpunk landscape layout (250x122)
            # Solid black title bar
            draw.rectangle((0, 0, width, 16), fill=0)
            draw.text((6, 2), "// MUJINA_SYS // v0.1.0", font=font_title, fill=255)
            
            # System status badge
            if status_text == "ONLINE":
                draw.rectangle((175, 2, 244, 14), fill=255)
                draw.text((184, 3), "[ ACTIVE_ON ]", font=font_sm, fill=0)
            else:
                draw.rectangle((175, 2, 244, 14), fill=255)
                draw.text((184, 3), "[ STBY_ERR ]", font=font_sm, fill=0)

            # Sep line
            draw.line((0, 18, width, 18), fill=0)
            
            # Vertical divider
            draw.line((95, 18, 95, 104), fill=0)

            # Left block: Hashrate
            draw.text((6, 22), "SPEED :", font=font_sm, fill=0)
            draw.text((6, 36), val_str, font=font_lg, fill=0)
            draw.rectangle((6, 75, 55, 87), fill=0)
            draw.text((12, 76), unit_str, font=font_sm, fill=255)

            # Right block: Stats
            draw.text((102, 22), f"NET  [ {ip_addr} ]", font=font_body, fill=0)
            draw.text((102, 42), f"NODE [ {boards_online:02d} ACTIVE ]", font=font_body, fill=0)
            draw.text((102, 62), f"TIME [ {uptime} ]", font=font_body, fill=0)
            draw.text((102, 82), f"POOL [ {pool} ]", font=font_body, fill=0)

            # Footer sep
            draw.line((0, 104, width, 104), fill=0)
            # Footer details
            draw.text((6, 107), "PORT: 7785 // SEC_CONN // HASH_STREAM", font=font_sm, fill=0)

        else:
            # Cyberpunk portrait layout (122x250)
            draw.rectangle((0, 0, width, 18), fill=0)
            draw.text((4, 3), "// MUJINA_OS", font=font_title, fill=255)
            
            draw.line((0, 19, width, 19), fill=0)

            # Upper block: Hashrate
            draw.text((4, 24), "SPEED:", font=font_sm, fill=0)
            draw.text((4, 36), val_str, font=font_lg, fill=0)
            draw.rectangle((4, 72, 50, 84), fill=0)
            draw.text((10, 73), unit_str, font=font_sm, fill=255)

            # Divider
            draw.line((0, 92, width, 92), fill=0)

            # Lower block: Stats
            draw.text((4, 98), f"ST: [ {status_text} ]", font=font_body, fill=0)
            draw.text((4, 118), f"IP: {ip_addr}", font=font_body, fill=0)
            draw.text((4, 138), f"BD: [ {boards_online:02d} ]", font=font_body, fill=0)
            draw.text((4, 158), f"UP: {uptime}", font=font_body, fill=0)
            draw.text((4, 178), f"PL: {pool}", font=font_sm, fill=0)

            # Footer
            draw.line((0, 232, width, 232), fill=0)
            draw.text((4, 235), "SYS_ON // SPI", font=font_sm, fill=0)

        # Draw buffer to screen
        try:
            epd.display(epd.getbuffer(image))
        except AttributeError:
            epd.display(image)

        time.sleep(REFRESH_INTERVAL)

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("EPD script terminated.")
        sys.exit(0)
