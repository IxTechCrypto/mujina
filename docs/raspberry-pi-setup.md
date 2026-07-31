# Raspberry Pi Setup & Deployment Guide for Mujina

This guide outlines the steps required to format, configure, compile, and run the Mujina miner daemon (`mujina-minerd`), web dashboard, and e-paper status monitor on a Raspberry Pi 4 or 5 driving Bitaxe and NerdQAxe++ boards.

---

## 1. Format & Flash the MicroSD Card

To run the miner daemon efficiently and support modern compiler toolchains, use **Raspberry Pi OS Lite (64-bit)** (based on Debian Bookworm or newer). The 64-bit version is required for modern Rust compilation targets.

1. **Download Raspberry Pi Imager:**
   - Download the official installer from [raspberrypi.com/software](https://www.raspberrypi.com/software).
2. **Choose OS:**
   - Select **Raspberry Pi OS (other)** -> **Raspberry Pi OS Lite (64-bit)**.
3. **Choose Storage:**
   - Insert your MicroSD card and select it.
4. **Pre-Configure Settings (OS Customization):**
   - Click the gear icon to open settings.
   - Set the hostname (e.g., `mujina-pi`).
   - Enable SSH (using password-based authentication or your SSH key).
   - Set a username and password (e.g., username `mujina`).
   - Configure wireless LAN (if not using wired Ethernet).
5. **Flash:**
   - Click **Write**.

---

## 2. Install OS Dependencies & Troubleshooting

Once the Pi is flashed and booted, SSH into it (`ssh mujina@mujina-pi.local`) and install the required build packages.

### Debian Trixie / Package List Troubleshooting
If you are running the Debian Trixie (testing) release, a fresh install may run into corrupted 0-byte repository lists that cause `apt update` or package installs to fail with "no installation candidate" errors. Fix this first:

```bash
# Clean up any corrupt package lists
sudo rm -rf /var/lib/apt/lists/*

# Perform a clean repository update and system upgrade
sudo apt update && sudo apt upgrade -y
```

### Install Build Dependencies
`libudev-dev` is critical for USB hotplug and device interface detection (relying on `udev` via the `nusb` crate). `iptables` may also be required if legacy networking configurations are used (since newer Debian versions default to `nftables`):

```bash
# Install core compiler and developer tools
sudo apt install -y build-essential libudev-dev libssl-dev pkg-config git python3 python3-pip python3-pil python3-requests iptables
```

---

## 3. Install Rust Toolchain

Compile the daemon natively on the Raspberry Pi using `rustup`:

```bash
# Install the Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Configure environment variables in current shell
source $HOME/.cargo/env
```

Verify the installation succeeded and reports a 64-bit target:
```bash
rustc --version
# Should output: rustc 1.x.y (aarch64-unknown-linux-gnu)
```

---

## 4. Clone and Compile Mujina

1. **Clone the Repository:**
   ```bash
   git clone https://github.com/IxTechCrypto/mujina.git
   cd mujina
   ```
2. **Compile the Daemon:**
   > [!WARNING]
   > Compiling on a Raspberry Pi 4 (4 GB model) with all 4 cores active can trigger Out-Of-Memory (OOM) compiler crashes or complete system freezes during linking. 
   > 
   > **To prevent OOM errors:**
   > 
   > 1. Restrict compilation to **2 threads** to reduce peak memory consumption:
   >    ```bash
   >    cargo build --release -j 2
   >    ```
   > 2. Alternatively, increase swap file allocation:
   >    ```bash
   >    sudo dphys-swapfile swapoff
   >    sudo nano /etc/dphys-swapfile
   >    # Change CONF_SWAPSIZE=100 to CONF_SWAPSIZE=2048
   >    sudo dphys-swapfile setup
   >    sudo dphys-swapfile swapon
   >    ```

The compiled binary will be generated at `target/release/mujina-minerd`.

---

## 5. Configure Device Node Permissions (Udev Rules)

By default, non-root users are blocked from accessing raw USB serial interface nodes (`/dev/ttyACM*`). Create a udev rule to automatically grant read/write access.

1. **Create the Rule File:**
   ```bash
   sudo nano /etc/udev/rules.d/99-mujina.rules
   ```
2. **Add the rules matching Bitaxe and NerdQAxe++ USB interfaces:**
   ```udev
   # NerdQAxe++ (PID 0xcaf1)
   SUBSYSTEM=="tty", ATTRS{idVendor}=="c0de", ATTRS{idProduct}=="caf1", MODE="0666", GROUP="dialout"
   
   # Bitaxe Gamma / NerdAxe (PID 0xcafe)
   SUBSYSTEM=="tty", ATTRS{idVendor}=="c0de", ATTRS{idProduct}=="cafe", MODE="0666", GROUP="dialout"
   ```
3. **Reload Udev Daemon:**
   ```bash
   sudo udevadm control --reload-rules
   sudo udevadm trigger
   ```
4. **Add your user to the dialout group:**
   ```bash
   sudo usermod -aG dialout $USER
   ```
   > [!IMPORTANT]
   > Log out and log back into your SSH session for the group membership updates to take effect.

---

## 6. Waveshare E-Paper Display Setup

The repository includes a telemetry HUD script in `scripts/epaper/mujina_epaper.py` targeting the **Waveshare 2.13" E-Ink HAT (V4, 250x122)**.

### SPI & GPIO Interface Enable
Enable the Raspberry Pi hardware SPI interface:
1. Run the interactive config utility:
   ```bash
   sudo raspi-config
   ```
2. Navigate to **Interface Options** -> **SPI** -> Select **Yes** to enable.
3. Exit and reboot the Pi:
   ```bash
   sudo reboot
   ```

### Python Display Library & Drivers Installation
1. Install low-level SPI and GPIO libraries:
   ```bash
   pip3 install spidev RPi.GPIO
   ```
2. Clone the official Waveshare e-paper Python library:
   ```bash
   git clone https://github.com/waveshare/e-Paper.git ~/e-Paper
   ```
3. Symlink the `waveshare_epd` driver package folder into the script directory:
   ```bash
   ln -s ~/e-Paper/RaspberryPi_JetsonNano/python/lib/waveshare_epd ~/mujina/scripts/epaper/waveshare_epd
   ```
4. Open the script file `~/mujina/scripts/epaper/mujina_epaper.py` and set the `EPD_MODEL` variable at the top to match your display version (e.g. `epd2in13_V4`).

---

## 7. Auto-Start on Boot (systemd)

A systemd service file (`mujina.service`) is included in the repository. It launches `start-all.sh` on boot, which starts the miner daemon, web dashboard, and e-paper display together.

### Install & Enable the Service
```bash
# 1. Copy the service file into systemd
sudo cp ~/mujina/mujina.service /etc/systemd/system/

# 2. Reload systemd so it picks up the new service
sudo systemctl daemon-reload

# 3. Enable it to start on every boot
sudo systemctl enable mujina

# 4. Start it right now
sudo systemctl start mujina
```

### Managing the Service
```bash
# Check status
sudo systemctl status mujina

# View live logs
journalctl -u mujina -f

# Restart
sudo systemctl restart mujina

# Stop
sudo systemctl stop mujina
```

---

## 8. Startup Scripts Reference

| Script | Purpose |
|---|---|
| `start-all.sh` | Starts the miner daemon, web dashboard (port 8088), and e-paper display |
| `start-mujina.sh` | Starts the miner daemon and e-paper display only |
| `stop-mujina.sh` | Gracefully stops the miner daemon and e-paper display |
| `dashboard.py` | Web dashboard server (port 8088) |
| `mujina.service` | systemd unit file for auto-start on boot |
