#!/bin/sh
# Autolaunch supervisor for the custom rtos_core/mujina stack (Linux
# side). Runs under rcS, which uses a real busybox ash shell -- while
# loops work here, unlike RT-Smart's msh (see main.c's --ipc6loop
# comment for why the RTOS side has to loop internally in C instead).
#
# rtos_core.elf itself is launched separately by /sharefs/init.sh on
# the RTOS/big-core side (--ipc6loop, loops forever internally). This
# script only supervises the Linux-side processes.
APP=/mntapp/release/linux/app

# Ensure /tmp directory exists for transient runtime logs (RAM-based tmpfs).
# Logging to /tmp prevents high-frequency wear on SPI NAND flash (/mntapp UBIFS).
mkdir -p /tmp

# [1] Start UI & Front Panel buttons IMMEDIATELY so the display illuminates
# and accepts user input without waiting for network/DHCP.
(
    while true; do
        /data/nano3s_ui > /tmp/nano3s_ui.log 2>&1
        sleep 3
    done
) &

(
    while true; do
        $APP/fb_button > /tmp/fb_button.log 2>&1
        sleep 3
    done
) &

# [2] Start BLE WiFi-setup provisioning server IMMEDIATELY so unprovisioned
# devices advertise as nan3s_XXXX right after Linux boots.
# Speaks raw HCI/ATT directly (no BlueZ/D-Bus) via HCI_CHANNEL_USER.
# Binary exits cleanly after 60s idle once wlan0 already has an IP address.
(
    while true; do
        hciconfig hci0 down 2>/dev/null
        /data/ble_setup > /tmp/ble_setup.log 2>&1
        sleep 3
    done
) &

# [3] Start power/fan idle-control harness (GPIO34 + fan pwm4 sysfs control)
(
    while true; do
        $APP/mujina_test_harness > /tmp/harness.log 2>&1
        sleep 3
    done
) &

# [4] Core 0 Miner daemon:
# Gated on wlan0 acquiring an IP address and rtos_core IPC readiness.
# Does not block UI, button, or BLE provisioning.
(
    # Wait for wlan0 IP address before starting miner daemon
    while true; do
        ip=$(ifconfig wlan0 2>/dev/null | grep 'inet addr:' | cut -d: -f2 | cut -d' ' -f1)
        if [ -n "$ip" ]; then
            break
        fi
        sleep 2
    done

    # Give rtos_core.elf a moment to register its IPC service before first connect
    sleep 3

    while true; do
        MUJINA_NANO3S_ENABLE=1 MUJINA_NANO3S_POWER_TARGET_W=62.0 MUJINA_API_LISTEN=0.0.0.0:80 MUJINA_POOL_URL=stratum+tcp://pool.256foundation.org:3333 MUJINA_POOL_USER=YOUR_WALLET.YOUR_WORKER /data/mujina-minerd > /data/mujina-minerd.log 2>&1
        sleep 3
    done
) &

# [5] Log size watchdog:
# Checks mujina-minerd.log every 5 minutes; if > 5MB, restart process to truncate cleanly.
(
    while true; do
        sleep 300
        if [ -f /data/mujina-minerd.log ]; then
            SIZE=$(wc -c /data/mujina-minerd.log 2>/dev/null | awk '{print $1}')
            if [ -n "$SIZE" ] && [ "$SIZE" -gt 5242880 ]; then
                PID=$(pidof mujina-minerd)
                if [ -n "$PID" ]; then
                    kill "$PID"
                fi
            fi
        fi
    done
) &

# [6] Hardware stall & IPC recovery watchdog:
# If RTOS detected a genuine stall and flagged /sharefs/reboot_request,
# or if mujina-minerd has been stuck blocked connecting to rtos_core for >=120s,
# execute a full hardware reboot.
#
# GUARDS:
# - Do NOT reboot if Wi-Fi is unprovisioned (user is onboarding via BLE).
# - Do NOT reboot if wlan0 has no IP address yet.
(
    ipc_blocked_counter=0
    while true; do
        sleep 5
        if [ -f /sharefs/reboot_request ]; then
            echo "[watchdog] /sharefs/reboot_request detected -- forcing reboot" >> /data/mujina-minerd.log
            rm -f /sharefs/reboot_request
            sync
            /sbin/reboot -f
        fi

        # Check provisioning state and network connectivity
        has_ssid=$(grep -E '^[[:space:]]*ssid=' /data/userconfig/wpa_supplicant.conf 2>/dev/null)
        wlan_ip=$(ifconfig wlan0 2>/dev/null | grep 'inet addr:' | cut -d: -f2 | cut -d' ' -f1)

        # Only evaluate IPC stall if provisioned and on the network
        if [ -n "$has_ssid" ] && [ -n "$wlan_ip" ]; then
            if tail -n 10 /data/mujina-minerd.log 2>/dev/null | grep -q "connecting to rtos_core (blocking)"; then
                ipc_blocked_counter=$((ipc_blocked_counter + 5))
                if [ $ipc_blocked_counter -ge 120 ]; then
                    echo "[watchdog] mujina-minerd stuck blocked connecting to rtos_core for >=120s -- forcing reboot" >> /data/mujina-minerd.log
                    sync
                    /sbin/reboot -f
                fi
            else
                ipc_blocked_counter=0
            fi
        else
            ipc_blocked_counter=0
        fi
    done
) &
