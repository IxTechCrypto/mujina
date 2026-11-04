#!/usr/bin/env python3
"""
Mujina Auto-Tuner Multi-Profile Benchmark Runner
Compares:
  - Old Baseline Codebase: 192.168.4.61 (Read-Only Firmware)
  - Upgraded Pareto Codebase: 192.168.4.29 (New Dual-Axis Tuner)

Profiles tested sequentially:
  1. Balanced  (2 hours initial baseline soak)
  2. Efficient (2 hours)
  3. MaxHash   (2 hours)

Records high-resolution telemetry every 30s into CSV and JSON.
"""

import os
import sys
import time
import json
import csv
import datetime
import urllib.request
import urllib.error

MINERS = {
    "old_baseline": {
        "ip": "192.168.4.61",
        "label": "Old Code (Baseline)",
    },
    "new_pareto": {
        "ip": "192.168.4.29",
        "label": "New Code (Pareto Tuner)",
    }
}

PROFILES = ["balanced", "efficient", "max_hash"]
PROFILE_DURATION_SECS = 2 * 3600  # 2 hours per profile
POLL_INTERVAL_SECS = 30           # Sample every 30 seconds

CSV_PATH = os.path.join(os.path.dirname(__file__), "benchmark_telemetry.csv")
SUMMARY_PATH = os.path.join(os.path.dirname(__file__), "benchmark_summary.json")

def http_get_json(ip, endpoint, timeout=5):
    url = f"http://{ip}/api/v0/{endpoint}"
    req = urllib.request.Request(url, headers={"User-Agent": "mujina-benchmark/1.0"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())

def http_patch_profile(ip, profile, timeout=5):
    url = f"http://{ip}/api/v0/boards/nano3s-0/tuner"
    body = json.dumps({"profile": profile}).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        headers={"Content-Type": "application/json", "User-Agent": "mujina-benchmark/1.0"},
        method="PATCH"
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.status

def init_csv():
    headers = [
        "timestamp_iso",
        "epoch_secs",
        "test_profile",
        "profile_elapsed_mins",
        "miner_id",
        "miner_ip",
        "tuner_phase",
        "freq_mhz",
        "voltage_mv",
        "hashrate_ths",
        "power_w",
        "efficiency_j_th",
        "asic_temp_c",
        "outlet_temp_c",
        "fan_pct",
        "fan_rpm",
        "last_event"
    ]
    if not os.path.exists(CSV_PATH) or os.path.getsize(CSV_PATH) == 0:
        with open(CSV_PATH, "w", newline="", encoding="utf-8") as f:
            writer = csv.writer(f)
            writer.writerow(headers)

def poll_miner(key, cfg, current_profile, profile_start_time):
    ip = cfg["ip"]
    now_iso = datetime.datetime.now().isoformat()
    now_epoch = time.time()
    elapsed_mins = round((now_epoch - profile_start_time) / 60.0, 2)
    
    try:
        tuner = http_get_json(ip, "boards/nano3s-0/tuner")
        board = http_get_json(ip, "boards/nano3s-0")
        
        phase = tuner.get("phase", "unknown")
        setpoint = tuner.get("setpoint") or {}
        freq = setpoint.get("frequency_mhz", 0.0)
        volt = setpoint.get("core_voltage_mv", 0)
        last_event = (tuner.get("last_event") or "").replace(",", ";")
        
        # Board metrics
        hr_raw = board.get("threads", [{}])[0].get("hashrate", 0)
        hashrate_ths = round(hr_raw / 1e12, 4)
        
        power_w = 0.0
        for p in board.get("powers", []):
            if p.get("name") == "core":
                power_w = round(p.get("power_w", 0.0), 2)
                break
        if power_w == 0.0 and board.get("powers"):
            power_w = round(board["powers"][0].get("power_w", 0.0), 2)
            
        efficiency = round(power_w / hashrate_ths, 2) if hashrate_ths > 0.1 else 0.0
        
        asic_temp = 0.0
        outlet_temp = 0.0
        for t in board.get("temperatures", []):
            if t.get("name") == "asic":
                asic_temp = round(t.get("temperature_c", 0.0), 1)
            elif t.get("name") == "outlet":
                outlet_temp = round(t.get("temperature_c", 0.0), 1)
                
        fan_pct = 0
        fan_rpm = 0
        fans = board.get("fans", [])
        if fans:
            fan_pct = fans[0].get("percent", 0)
            fan_rpm = fans[0].get("rpm", 0)
            
        row = [
            now_iso,
            int(now_epoch),
            current_profile,
            elapsed_mins,
            key,
            ip,
            phase,
            freq,
            volt,
            hashrate_ths,
            power_w,
            efficiency,
            asic_temp,
            outlet_temp,
            fan_pct,
            fan_rpm,
            last_event
        ]
        
        with open(CSV_PATH, "a", newline="", encoding="utf-8") as f:
            writer = csv.writer(f)
            writer.writerow(row)
            
        return {
            "ok": True,
            "ip": ip,
            "key": key,
            "phase": phase,
            "freq": freq,
            "volt": volt,
            "hashrate_ths": hashrate_ths,
            "power_w": power_w,
            "efficiency": efficiency,
            "asic_temp": asic_temp,
            "outlet_temp": outlet_temp,
            "fan_pct": fan_pct,
            "fan_rpm": fan_rpm
        }
    except Exception as e:
        return {"ok": False, "ip": ip, "key": key, "error": str(e)}

def update_summary(summary_data):
    with open(SUMMARY_PATH, "w", encoding="utf-8") as f:
        json.dump(summary_data, f, indent=2)

def main():
    print("=" * 70)
    print("Mujina Auto-Tuner Multi-Profile Benchmark Runner Starting")
    print(f"Profiles to test: {PROFILES}")
    print(f"Duration per profile: {PROFILE_DURATION_SECS / 3600:.1f} hours ({PROFILE_DURATION_SECS} seconds)")
    print(f"Poll interval: {POLL_INTERVAL_SECS} seconds")
    print(f"Output CSV: {CSV_PATH}")
    print(f"Output Summary: {SUMMARY_PATH}")
    print("=" * 70)
    
    init_csv()
    
    overall_start = time.time()
    summary = {
        "benchmark_start_iso": datetime.datetime.now().isoformat(),
        "profiles": {},
        "status": "in_progress"
    }
    
    for profile in PROFILES:
        print(f"\n[>>>] BEGINNING PROFILE TEST: {profile.upper()}")
        print(f"Setting profile '{profile}' on both miners...")
        for key, cfg in MINERS.items():
            ip = cfg["ip"]
            try:
                status = http_patch_profile(ip, profile)
                print(f"  [+] {key} ({ip}): Profile set to '{profile}' (HTTP {status})")
            except Exception as e:
                print(f"  [!] {key} ({ip}): Failed to set profile: {e}")
                
        profile_start = time.time()
        samples = {k: [] for k in MINERS.keys()}
        
        summary["profiles"][profile] = {
            "start_iso": datetime.datetime.now().isoformat(),
            "target_duration_secs": PROFILE_DURATION_SECS,
            "completed": False
        }
        update_summary(summary)
        
        last_print = 0
        while True:
            now = time.time()
            elapsed = now - profile_start
            if elapsed >= PROFILE_DURATION_SECS:
                print(f"\n[v] Completed profile {profile.upper()} ({elapsed/3600:.2f} hours)")
                break
                
            # Sample each miner
            results = {}
            for key, cfg in MINERS.items():
                res = poll_miner(key, cfg, profile, profile_start)
                results[key] = res
                if res.get("ok"):
                    samples[key].append(res)
                    
            # Periodic console log (every 60s)
            if now - last_print >= 60:
                mins_left = (PROFILE_DURATION_SECS - elapsed) / 60.0
                r_old = results.get("old_baseline", {})
                r_new = results.get("new_pareto", {})
                print(f"[{datetime.datetime.now().strftime('%H:%M:%S')}] [{profile.upper()}] Remaining: {mins_left:.1f}m | "
                      f"OLD: {r_old.get('hashrate_ths', 0):.2f}TH/s, {r_old.get('power_w', 0):.1f}W, {r_old.get('efficiency', 0):.2f}J/TH, {r_old.get('asic_temp', 0):.1f}C, {r_old.get('freq', 0):.0f}MHz | "
                      f"NEW: {r_new.get('hashrate_ths', 0):.2f}TH/s, {r_new.get('power_w', 0):.1f}W, {r_new.get('efficiency', 0):.2f}J/TH, {r_new.get('asic_temp', 0):.1f}C, {r_new.get('freq', 0):.0f}MHz")
                last_print = now
                
            # Update summary stats
            for key in MINERS.keys():
                valid = [s for s in samples[key] if s.get("ok") and s.get("hashrate_ths", 0) > 0.5]
                if valid:
                    avg_hr = sum(s["hashrate_ths"] for s in valid) / len(valid)
                    avg_pw = sum(s["power_w"] for s in valid) / len(valid)
                    avg_eff = avg_pw / avg_hr if avg_hr > 0 else 0
                    avg_temp = sum(s["asic_temp"] for s in valid) / len(valid)
                    summary["profiles"][profile][key] = {
                        "samples_count": len(valid),
                        "avg_hashrate_ths": round(avg_hr, 3),
                        "avg_power_w": round(avg_pw, 2),
                        "avg_efficiency_j_th": round(avg_eff, 2),
                        "avg_asic_temp_c": round(avg_temp, 1),
                        "last_freq_mhz": valid[-1].get("freq", 0),
                        "last_volt_mv": valid[-1].get("volt", 0)
                    }
            update_summary(summary)
            
            time.sleep(POLL_INTERVAL_SECS)
            
        summary["profiles"][profile]["completed"] = True
        summary["profiles"][profile]["end_iso"] = datetime.datetime.now().isoformat()
        update_summary(summary)
        
    summary["status"] = "completed"
    summary["benchmark_end_iso"] = datetime.datetime.now().isoformat()
    update_summary(summary)
    print("\n" + "=" * 70)
    print("ALL PROFILES COMPLETED SUCCESSFULLY!")
    print(f"Full CSV telemetry at: {CSV_PATH}")
    print(f"Summary JSON at: {SUMMARY_PATH}")
    print("=" * 70)

if __name__ == "__main__":
    main()
