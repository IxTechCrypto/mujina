# Mujina on Avalon Nano 3S: Complete Installation & Walkthrough Guide

> [!IMPORTANT]
> ### ⚡ Release V5 Firmware (Thermal Slew-Rate Pacing, Autonomous Self-Healing & BLE Onboarding)
> This guide covers the **V5 production release** (`nano-mujina-v5-release.kdimg` and `nano-mujina-v5-bundle.bin`).
> Key features included in this build:
> - **Thermal Slew-Rate Pacing (`15s` check interval)**: Eliminates analog PLL frequency shock cascades and prevents ASIC daisy-chain stalls.
> - **Pre-emptive Fan Ramp Boost**: 2.5x dynamic fan ramp within 2.0°C of the thermal ceiling to arrest temperature spikes.
> - **Autonomous Self-Healing Watchdog**: Cross-core signaling (`/sharefs/reboot_request` + 60s IPC freeze detection) triggers an automatic hardware reboot within 25s without human intervention.
> - **Fixed Out-of-the-Box BLE Wi-Fi Setup**: Full Bluetooth LE GATT advertising as `nan3s_XXXX` with robust Wi-Fi scanning (`iwlist` fallback, RSSI descending sort, auto-DHCP via `udhcpc`), compatible with the official Canaan Avalon Family / Avalon Life mobile app.
> - **Dual-Axis Pareto Auto-Tuner**: Dynamic frequency stepping, per-chip PLL offset optimization, and continuous DPS mode.

---

## Part 1: Windows Installation Walkthrough (Zero Python Required)

You have two 100% Python-free flashing methods on Windows. Neither requires installing Python, setting environment variables, or touching the Windows Command Prompt.

```
                         Choose Your Windows Method
                                     │
         ┌───────────────────────────┴───────────────────────────┐
         ▼                                                       ▼
   Method 1: Torii App                                    Method 2: Canaan Burning Tool
   • Modern 1-Click GUI                                   • Official Vendor Utility
   • Bundled private flasher engine                       • Zero driver setup
   • Auto-cleans flash partitions                         • Standalone .zip portable
   • Integrated dashboard handoff                         • Direct SPI NAND burning
```

---

### Method 1: Using Torii (Recommended — 1-Click Automated GUI)

Torii is the unified provisioner for Mujina. It bundles a private, self-contained Python runtime and `k230-flash` flasher engine inside its application directory.

#### What You Need:
1. **Torii App (Latest Build):**
   * **Standalone Executable:** [`torii.exe`](https://github.com/IxTechCrypto/torii)
2. **Mujina Release Image:**
   * **Full Flash Image:** `nano-mujina-v5-release.kdimg` (129,232,128 bytes)
   * **Network OTA Bundle (for existing users):** `nano-mujina-v5-bundle.bin` (19,560,704 bytes)
3. **Hardware & Cables:**
   * **Male USB-A to Male USB-A data cable**.
   * A paperclip, SIM ejector tool, or pin.
   * Factory 28V DC power adapter (or Mean Well 200-24 @ 28V).
4. **WinUSB Driver (One-Time Setup with Zadig):**
   * Download [Zadig 2.9 (Portable)](https://zadig.akeo.ie/).

---

#### Step-by-Step Walkthrough:

#### Step 1: Connect Hardware in Burn Mode
1. Unplug the factory Wi-Fi USB dongle from the Avalon Nano 3S.
2. Plug one end of your **USB-A to USB-A cable** into your Windows PC.
3. Plug the other end into the USB-A port of the Nano 3S (where the Wi-Fi dongle was).
4. Locate the **recessed recovery button pinhole** next to the USB port.
5. With your paperclip, **press and gently hold the recovery button**.
6. While keeping the button pressed, **insert the 28V DC barrel jack** from the power supply.
7. Keep holding the button for **3 full seconds**, then release it.
   > The device is now in K230 ROM Bootloader mode (`VID 29F1: PID 0230`). The front screen remains dark and the fan runs at low idle speed.

#### Step 2: Configure USB Driver (First Time Only)
1. Open **Zadig**.
2. Click **Options** → check **List All Devices**.
3. In the dropdown, select **K230 USB Boot Device** (or "Unknown Device" with Hardware ID `29F1:0230`).
4. On the right side of the green arrow, select **WinUSB**.
5. Click **Install Driver** (or **Replace Driver**). Wait ~15 seconds until it completes.
6. Close Zadig. (You will never need to do this step again on this PC).

#### Step 3: Flash with Torii
1. Launch `torii.exe`.
2. On the left navigation bar, click **Hardware Workflows** → select **Avalon Nano 3S**.
3. Torii will detect your connected device immediately:  
   🟢 `K230 Boot Device Ready (29f1:0230)`.
4. Click **Select Image** and browse to `nano-mujina-v5-release.kdimg`.
5. Click **Flash Device**.
6. **Watch the live progress:**
   * Torii erases stale flash blocks and streams the 11 partitions (including `app_ubi` and `data`).
   * Flashing takes approximately **2 to 3 minutes**.
7. When Torii reports **"Flashing Complete"**, unplug the 28V DC power jack.
8. Unplug the USB-A to USB-A cable.
9. **Plug your factory Wi-Fi USB dongle back into the Nano 3S.**

---

### Method 2: Without Torii (Official Canaan Burning Tool — Zero Setup)

If you prefer using Canaan’s native standalone vendor utility, it runs as a portable Windows C++ application with no driver installation required:

* **Python Required:** **NO**
* **Command Prompt Required:** **NO**
* **Zadig Setup Required:** **NO**

#### Step-by-Step Walkthrough:

1. **Download the Canaan Burning Tool:**
   * Download [K230BurningTool-Windows-v2.1.0.zip](https://kendryte-download.canaan-creative.com/k230/downloads/burn_tool/v2.1.0/K230BurningTool-Windows-v2.1.0-0-gd24909e.zip).
   * Extract the `.zip` archive.
2. **Download Firmware:**
   * Locate `nano-mujina-v5-release.kdimg`.
3. **Connect in Burn Mode:**
   * Unplug Wi-Fi dongle, insert USB-A to USB-A cable to PC.
   * Hold recovery pin, insert 28V DC power, hold 3 seconds, release.
4. **Launch & Burn:**
   * Run `K230BurningTool.exe`.
   * Verify the debug pane says: `Device Attached: 29f1:0230`.
   * Click **Open** and select `nano-mujina-v5-release.kdimg`.
   * Configure dropdowns:
     * **Image part name:** `all`
     * **Medium:** `SPI NAND`
   * Click **Start**.
   * Wait 2–3 minutes until the status indicator turns bright green: **"Downloading Completed"**.
5. **Finish Up:**
   * Unplug DC power and USB cable.
   * **Reinsert the Wi-Fi USB dongle.**
   * Power the device back on.

---

## Part 2: Upgrading Existing Miners Over Network (No Cables Needed)

If your miner is already running Mujina, you can update without physical cables:

### Option A: Via the Web Dashboard
1. Open your miner's dashboard in any web browser (`http://<MINER_IP>/`).
2. Go to the **Firmware** or **Update** tab (`http://<MINER_IP>/firmware`).
3. Select `nano-mujina-v5-bundle.bin` and click **Upload & Update**.
4. The miner validates the `MJBUNDL1` bundle signature, applies the new binaries atomically, and reboots automatically within 25 seconds.

### Option B: Via `curl` Command Line
```bash
curl -X POST --data-binary @nano-mujina-v5-bundle.bin \
  -H "Content-Type: application/octet-stream" \
  http://<MINER_IP>/api/v0/firmware/bundle
```

---

## Part 3: First Boot, Wi-Fi Onboarding & Mining Setup

### 1. Initial Power-Up
1. Ensure the **Wi-Fi USB dongle** is plugged into the Nano 3S.
2. Plug in the 28V DC power supply.
3. The front status screen will illuminate with the Mujina boot splash and initialize both K230 cores (Core 1 RT-Smart ASIC engine + Core 0 Linux).

### 2. Connect Wi-Fi via Avalon Mobile App (Bluetooth LE)
1. If Wi-Fi is not yet configured, the miner automatically enters Bluetooth LE setup mode, advertising as **`nan3s_XXXX`** (where `XXXX` is its unique device ID).
2. Open the official **Avalon Family** or **Avalon Life** app on your iPhone or Android phone.
3. Tap **Add Device** / **Setup Wi-Fi**.
4. The app will immediately discover `nan3s_XXXX` via Bluetooth.
5. Select your local 2.4 GHz Wi-Fi network from the list (scanned and sorted by signal strength).
6. Enter your Wi-Fi password and tap **Connect**.
7. The miner LCD will display setup progress (`LINK` -> `SAVE` -> `OK`), acquire a DHCP lease, and display its assigned LAN IP address.

### 3. Open the Mujina Cyberpunk Web Dashboard
1. From any computer or phone on the same network, navigate to:
   ```text
   http://<MINER_IP>/
   ```
2. **Configure Stratum Pool:**
   * Click the **Pool** menu in the top navigation bar.
   * **Stratum URL:** e.g. `stratum+tcp://pool.256foundation.org:3333` or `stratum+tcp://parasite.wtf:42069`
   * **Worker:** `your_btc_address.worker_name`
   * Click **Apply & Save**.
3. **Select Tuning Profile:**
   * In the **Tuning** sheet, choose:
     * **Quiet**: 65W power target, whisper-quiet fan.
     * **Efficient**: Deep undervolting maximizing Joules per Terahash ($J/\text{TH}$).
     * **Balanced**: Factory medium wattage with dynamic voltage/frequency optimization.
     * **Max Hash**: Climbs to full continuous safety envelope (~5.0+ TH/s @ 105–112W).
4. **Customize WS2812 LEDs:**
   * Open `http://<MINER_IP>/led` to select dynamic Stratum Share effects (`SharePulse`, `ShareWave`, `Cylon Scanner`, `Halloween`, `Christmas`). Settings persist non-volatilely to `/data/userconfig/led.conf`.
