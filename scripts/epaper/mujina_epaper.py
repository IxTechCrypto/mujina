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
    print("Install via: pip3 install waveshare-epd")
    sys.exit(1)

def main():
    print(f"Initializing Waveshare EPD: {EPD_MODEL}")
    epd = epd_module.EPD()
    epd.init()
    epd.Clear(0xFF) # Clear screen to white (0xFF)

    # Load standard DejaVu Sans fonts available on Raspberry Pi OS
    try:
        font_title = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 13)
        font_body = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 11)
        font_sm = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 9)
    except IOError:
        font_title = font_body = font_sm = ImageFont.load_default()

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
                
                # Fetch fleet hashrate if multi-board, else single board fallback
                if "fleet" in data:
                    hashrate = f"{data['fleet'].get('hashrate_ths', 0.0):.2f} TH/s"
                    boards_online = data["fleet"].get("boards_online", 0)
                elif "hashrate_ths" in data:
                    hashrate = f"{data.get('hashrate_ths', 0.0):.2f} TH/s"
                    boards_online = 1
                
                uptime = data.get("uptime", "0h 0m")
                pool = data.get("pool_name", "N/A")
                if len(pool) > 24:
                    pool = pool[:22] + "..."
        except Exception:
            status_text = "DAEMON OFFLINE"

        # Adaptive layout rendering depending on display aspect ratio orientation
        is_landscape = width > height
        if is_landscape:
            # Header
            draw.rectangle((0, 0, width, 18), fill=0) # Black title bar
            draw.text((4, 2), "MUJINA // FLEET STATUS", font=font_title, fill=255)

            # Details
            draw.text((4, 22), f"Daemon:  {status_text}", font=font_body, fill=0)
            draw.text((4, 38), f"IP Addr: {ip_addr}", font=font_body, fill=0)
            draw.text((4, 54), f"Hashrate: {hashrate}", font=font_body, fill=0)
            draw.text((4, 70), f"Boards:  {boards_online} Active", font=font_body, fill=0)
            draw.text((4, 86), f"Uptime:  {uptime}", font=font_body, fill=0)

            # Footer
            draw.line((0, 103, width, 103), fill=0)
            draw.text((4, 105), f"Pool: {pool}", font=font_sm, fill=0)
        else:
            # Portrait layout
            draw.rectangle((0, 0, width, 18), fill=0)
            draw.text((2, 2), "MUJINA", font=font_title, fill=255)

            draw.text((2, 24), f"ST: {status_text}", font=font_body, fill=0)
            draw.text((2, 40), f"IP: {ip_addr}", font=font_body, fill=0)
            draw.text((2, 56), f"HR: {hashrate}", font=font_body, fill=0)
            draw.text((2, 72), f"BD: {boards_online}", font=font_body, fill=0)
            draw.text((2, 88), f"UP: {uptime}", font=font_body, fill=0)
            draw.text((2, 105), f"PL: {pool}", font=font_sm, fill=0)

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
