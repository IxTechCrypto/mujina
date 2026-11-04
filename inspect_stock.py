import socket
import json

ip = '192.168.4.67'

def send_cmd(cmd):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(3)
    s.connect((ip, 4028))
    msg = json.dumps({'command': cmd}) + '\n'
    s.sendall(msg.encode())
    data = b''
    while True:
        c = s.recv(4096)
        if not c:
            break
        data += c
    s.close()
    return json.loads(data.decode(errors='ignore').replace('\x00', ''))

print("--- SUMMARY ---")
summary = send_cmd('summary')
print(json.dumps(summary, indent=2))

print("\n--- STATS ---")
stats = send_cmd('stats')
print(json.dumps(stats, indent=2))

print("\n--- DEVS ---")
devs = send_cmd('devs')
print(json.dumps(devs, indent=2))
