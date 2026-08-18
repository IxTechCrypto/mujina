# Windows Setup & Deployment Guide for Mujina

This guide outlines the steps required to configure, compile, and run the Mujina miner daemon (`mujina-minerd`), web dashboard, and PowerShell helper scripts on Microsoft Windows driving Bitaxe and NerdQAxe++ boards.

## Quick Start: Running Pre-Built Binary (No Build Required)

If you have a pre-built release package or compiled `mujina-minerd.exe`, you do **not** need Rust, Cargo, or Visual Studio installed on your computer.

### 1. Requirements for Pre-Built Binary Users
- **Windows 10 / 11** (includes native USB serial drivers for Bitaxe and NerdQAxe++).
- **Python 3.10+** (only if running the web dashboard interface). Download from [python.org](https://www.python.org/downloads/) or via `winget install Python.Python.3.12`.
- (Optional) [Visual C++ Redistributable](https://aka.ms/vs/17/release/vc_redist.x64.exe) if your Windows installation lacks standard C runtime libraries.

### 2. 1-Click Launch
1. Ensure `mujina-minerd.exe` is in the repository root folder (or `target\release\`).
2. Open PowerShell and run:
   ```powershell
   .\start-all.ps1
   ```
   Or double-click `start-all.ps1` in Windows Explorer.
3. Open `http://127.0.0.1:8088` in your web browser.

---

## Building from Source (Developer Setup)

If you are modifying the codebase or compiling Mujina yourself, follow the build requirements below:

### 1. Rust Toolchain
1. Download `rustup-init.exe` from [rustup.rs](https://rustup.rs).
2. Run the installer and select default installation option (`x86_64-pc-windows-msvc`).
3. Verify installation in PowerShell:
   ```powershell
   rustc --version
   cargo --version
   ```

### 2. Build Tools for Visual Studio
Rust requires the MSVC C++ linker (`link.exe`) provided by Microsoft Visual Studio.
1. Download **Build Tools for Visual Studio** from [visualstudio.microsoft.com/downloads](https://visualstudio.microsoft.com/downloads/).
2. In the installer, select the **Desktop development with C++** workload.
3. Ensure **MSVC v143 - VS 2022 C++ x64/x86 build tools** and **Windows 11 SDK** (or Windows 10 SDK) are checked.

### 3. Python 3 (For Web Dashboard)
1. Download Python 3.10+ from [python.org](https://www.python.org/downloads/) or via `winget`:
   ```powershell
   winget install Python.Python.3.12
   ```
2. **Important:** Check **"Add python.exe to PATH"** during installation.

### 4. Git & Optional Build Utilities
1. Install Git for Windows (`winget install Git.Git`).
2. (Optional) Install `just` command runner:
   ```powershell
   cargo install just
   ```

---

## 2. Hardware & Serial Driver Setup

Bitaxe Gamma and NerdQAxe++ boards communicate with Windows over USB CDC-ACM serial interfaces.

### USB CDC-ACM Driver Detection
- Modern Windows 10 and Windows 11 include standard `usbser.sys` CDC-ACM serial drivers built-in.
- When plugging in a Bitaxe or NerdQAxe++ board over USB, Windows automatically assigns Virtual COM Ports (e.g., `COM3`, `COM4`).
- Verification: Open **Device Manager** -> **Ports (COM & LPT)** to verify your board appears when plugged in.

### Hardware USB PnP & Power Rail Gotchas

> [!IMPORTANT]
> **Dual-Power Rail Reset (12V DC vs 5V USB VBUS):**
> Unplugging USB while external 12V DC power remains connected keeps the ESP32 powered on its 3.3V internal rail, but drops the 5V USB VBUS signal.
> Upon replugging USB, the ESP32 does not perform a cold boot on its own. To force Windows to re-initialize USB PnP and re-establish UART communication:
> 1. Press the physical **RESET** button on the ESP32 board, OR
> 2. Power-cycle the 12V DC power supply.

> [!WARNING]
> **Graceful Process Termination:**
> Force-killing `mujina-minerd.exe` (e.g. via Task Manager End Task or SIGKILL) while active ASIC hashing is occurring can lock the Win32 CDC-ACM serial handle in `usbser.sys`, requiring a physical USB replug.
> Always use `Ctrl+C` or `.\stop-mujina.ps1` to stop the daemon gracefully.

---

## 3. Building Mujina

1. Clone the repository:
   ```powershell
   git clone https://github.com/IxTechCrypto/mujina.git
   cd mujina
   ```
2. Build the workspace:
   ```powershell
   cargo build --release
   ```
   The compiled binary will be located at `target\release\mujina-minerd.exe`.

---

## 4. Running Mujina on Windows

### Option A: Using PowerShell Launch Scripts (Recommended)

Three helper scripts are provided in the repository root for seamless launching and process management on Windows:

| Script | Purpose |
|---|---|
| `.\start-all.ps1` | Boots both `mujina-minerd.exe` and `dashboard.py` in background windows |
| `.\start-mujina.ps1` | Starts `mujina-minerd.exe` with default API and pool settings |
| `.\stop-mujina.ps1` | Gracefully terminates running `mujina-minerd` and dashboard processes |

To start the full stack:
```powershell
.\start-all.ps1
```
Then open your browser to `http://127.0.0.1:8088`.

### Option B: PowerShell Command Line

Set environment variables in PowerShell using `$env:` syntax:

```powershell
# Set Pool Configuration
$env:MUJINA_POOL_URL="stratum+tcp://pool.example.com:3333"
$env:MUJINA_POOL_USER="your_wallet.worker1"
$env:MUJINA_POOL_PASS="x"

# Run Daemon
cargo run --bin mujina-minerd
```

### Option C: Command Prompt (CMD)

Set environment variables in CMD using `set` syntax:

```cmd
set MUJINA_POOL_URL=stratum+tcp://pool.example.com:3333
set MUJINA_POOL_USER=your_wallet.worker1
set MUJINA_POOL_PASS=x

cargo run --bin mujina-minerd
```

### CPU Mining Test Mode

To test Mujina on Windows without physical ASIC hardware connected:

```powershell
$env:MUJINA_CPUMINER_THREADS="1"
$env:MUJINA_CPUMINER_DUTY="50"
$env:MUJINA_USB_DISABLE="1"
cargo run --bin mujina-minerd
```

---

## 5. Web Dashboard Setup

The cyberpunk web dashboard is driven by a lightweight Python HTTP server (`dashboard.py`) proxying the Rust daemon's REST API (`http://127.0.0.1:7785/api/v0/miner`).

1. Launch `dashboard.py`:
   ```powershell
   python dashboard.py
   ```
2. Open `http://127.0.0.1:8088` in any browser.
3. The dashboard supports hot-reloading `dashboard.html` on browser refresh (`F5`) without restarting the Python process.
4. **Web UI Restart Integration:** Triggering a restart in the dashboard UI automatically invokes `stop-mujina.ps1` followed by `start-mujina.ps1`.
