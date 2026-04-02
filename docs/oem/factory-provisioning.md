# Factory Device Provisioning

Every unit needs a unique identity for cloud communication, security, and fleet management. This document covers how devices get their identity, how to provision at scale, and how to deprovision.

---

## 1. Device ID Generation

Each device gets a UUID v4 as its unique identifier.

**Default behavior (single units):**
- On first boot, if `/etc/ados/device-id` does not exist, the agent generates a UUID v4
- Written to `/etc/ados/device-id` (plain text, single line)
- Example: `a3f2b7c1-4d8e-4f9a-b2c1-7e8f9a0b1c2d`
- This ID is permanent unless factory reset is triggered

**Why UUID v4?**
- Globally unique without a central authority
- No coordination needed between factory lines
- 128-bit space makes collision effectively impossible
- Easy to generate offline

---

## 2. Certificate Enrollment

TLS certificates are used for MQTT connections and the REST API.

### Self-Signed (Default)

On first boot, the agent generates a self-signed certificate:

```
/etc/ados/certs/
├── device.key       # 2048-bit RSA private key
├── device.crt       # Self-signed certificate (valid 10 years)
└── ca.crt           # Altnautica CA certificate (for verifying cloud)
```

The self-signed cert is sufficient for encrypted communication. The MQTT broker authenticates via username/password, not client certificates.

### CA-Signed (Optional, Enterprise)

For higher security deployments, the device can send a Certificate Signing Request (CSR) to Altnautica's CA or your own CA:

```
First boot
  → Generate key pair
  → Create CSR with device ID as Common Name
  → POST CSR to https://ca.altnautica.com/sign (or your CA endpoint)
  → Receive signed certificate
  → Store in /etc/ados/certs/device.crt
```

CA-signed certificates enable mutual TLS (mTLS) authentication on the MQTT broker, eliminating the need for username/password auth.

**To use your own CA:** Set the CA endpoint in the board profile or config:

```yaml
security:
  ca_endpoint: "https://ca.yourdomain.com/sign"
  ca_cert: "/etc/ados/certs/your-ca.crt"
```

---

## 3. MQTT Credentials

### Username/Password (Default)

The device ID is used as the MQTT username. The password is derived from the device ID and a shared secret:

```
MQTT username: {deviceId}
MQTT password: HMAC-SHA256(shared_secret, deviceId)
```

The shared secret is baked into the production image at build time. All devices from the same OEM share the same secret. The MQTT broker must be configured with matching credentials.

**Generating the password file for Mosquitto:**

```bash
# generate-mqtt-passwords.sh
#!/bin/bash
SECRET="your-shared-secret-here"

while IFS= read -r device_id; do
  password=$(echo -n "$device_id" | openssl dgst -sha256 -hmac "$SECRET" -binary | base64)
  echo "$device_id:$password"
done < device-ids.txt | mosquitto_passwd -U /dev/stdin > passwords
```

### API Key (Enterprise)

For Enterprise deployments, each device gets a unique API key generated during provisioning:

```
Device ID:  a3f2b7c1-4d8e-4f9a-b2c1-7e8f9a0b1c2d
API Key:    ak_live_x7q9w2e4r6t8y0u1i3o5p7a9s1d3f5g7
```

API keys are stored in `/etc/ados/api-key` and used for both MQTT auth and REST API authentication.

---

## 4. Batch Provisioning (100+ units)

For production runs, pre-generate device identities and flash them alongside the OS image.

### Provisioning Script

```bash
#!/bin/bash
# provision-batch.sh
# Generates device IDs, certs, and MQTT credentials for a batch of units

BATCH_SIZE=${1:-100}
BATCH_NAME=${2:-"batch-$(date +%Y%m%d)"}
OUTPUT_DIR="./provisioning/$BATCH_NAME"
MQTT_SECRET="your-shared-secret"

mkdir -p "$OUTPUT_DIR/devices"

echo "Generating $BATCH_SIZE device identities..."

for i in $(seq 1 $BATCH_SIZE); do
  DEVICE_ID=$(uuidgen)
  DEVICE_DIR="$OUTPUT_DIR/devices/$DEVICE_ID"
  mkdir -p "$DEVICE_DIR/certs"

  # Write device ID
  echo "$DEVICE_ID" > "$DEVICE_DIR/device-id"

  # Generate TLS certificate
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "$DEVICE_DIR/certs/device.key" \
    -out "$DEVICE_DIR/certs/device.crt" \
    -days 3650 \
    -subj "/CN=$DEVICE_ID/O=YourOEM/C=CN" \
    2>/dev/null

  # Generate MQTT password
  MQTT_PASS=$(echo -n "$DEVICE_ID" | openssl dgst -sha256 -hmac "$MQTT_SECRET" -binary | base64)
  echo "$MQTT_PASS" > "$DEVICE_DIR/mqtt-password"

  echo "  [$i/$BATCH_SIZE] $DEVICE_ID"
done

# Generate MQTT password file for broker
echo "Generating Mosquitto password file..."
for dir in "$OUTPUT_DIR/devices"/*/; do
  DEVICE_ID=$(cat "$dir/device-id")
  MQTT_PASS=$(cat "$dir/mqtt-password")
  echo "$DEVICE_ID:$MQTT_PASS"
done > "$OUTPUT_DIR/mqtt-passwords.txt"

# Generate manifest
echo "device_id,serial_number,created" > "$OUTPUT_DIR/manifest.csv"
SERIAL=1
for dir in "$OUTPUT_DIR/devices"/*/; do
  DEVICE_ID=$(cat "$dir/device-id")
  echo "$DEVICE_ID,SN-$(printf '%06d' $SERIAL),$(date -Iseconds)" >> "$OUTPUT_DIR/manifest.csv"
  SERIAL=$((SERIAL + 1))
done

echo ""
echo "Done. Output in $OUTPUT_DIR/"
echo "  devices/       - Per-device identity files"
echo "  manifest.csv   - Batch manifest"
echo "  mqtt-passwords.txt - Import into Mosquitto"
```

### Flashing with Pre-Generated Identity

After flashing the base OS image, copy the device-specific files:

```bash
# For each unit on the flash station:
DEVICE_ID="a3f2b7c1-..."
DEVICE_DIR="./provisioning/batch-20260321/devices/$DEVICE_ID"

# Mount the device's root filesystem
mount /dev/sdX2 /mnt/device

# Copy identity files
mkdir -p /mnt/device/etc/ados/certs
cp "$DEVICE_DIR/device-id" /mnt/device/etc/ados/device-id
cp "$DEVICE_DIR/certs/"* /mnt/device/etc/ados/certs/

# Set permissions
chmod 600 /mnt/device/etc/ados/certs/device.key
chmod 644 /mnt/device/etc/ados/device-id

umount /mnt/device
```

For Rockchip FactoryTool: create per-device overlay images and add them as a post-flash step.

### Tracking

Keep the `manifest.csv` file. It maps device IDs to serial numbers and batch dates. Upload it to your fleet management system so devices show up with their serial numbers in the dashboard.

---

## 5. Fleet Enrollment

When the end user completes the setup wizard, the device registers itself with the cloud backend.

### Enrollment Flow

```
User completes setup wizard
  → Device has: WiFi connection + MQTT credentials + device ID
  → Agent sends enrollment request to cloud:
      POST /api/agent/enroll
      {
        "deviceId": "a3f2b7c1-...",
        "firmware": "0.3.1",
        "hardware": "hglrc-companion-v1",
        "fcType": "ArduPilot",
        "fcVersion": "4.5.7"
      }
  → Cloud creates device record in fleet database
  → Device appears in fleet dashboard
  → User can now claim the device to their account
```

### Device Claiming

A device can be claimed by a user through the GCS or fleet dashboard:

1. User opens ADOS Mission Control (web GCS)
2. Goes to Fleet > Add Device
3. Enters the device's claim code (shown on the config webapp or printed on a sticker)
4. Device is linked to the user's account

The claim code is derived from the device ID: first 8 characters, uppercase, with hyphens (e.g., `A3F2-B7C1`). Simple enough to type from a sticker.

---

## 6. Deprovisioning

### Factory Reset (End User)

Hold user button > 10 seconds:
- Deletes `/etc/ados/device-id`
- Deletes `/etc/ados/certs/`
- Deletes `/etc/ados/config.yaml`
- Clears MQTT credentials
- New identity generated on next boot
- Old device record in cloud becomes orphaned (marked inactive after 30 days)

### OEM Deprovisioning (Returns/Refurbs)

For returned units going back into inventory:

```bash
# SSH into device
ssh root@192.168.4.1

# Full deprovision
ados deprovision --confirm

# This does everything factory reset does, PLUS:
# - Clears flight logs
# - Resets boot count
# - Removes any user-installed scripts
# - Notifies cloud to delete device record immediately
```

### Cloud-Side Cleanup

If a device is deprovisioned but the cloud record persists:

```bash
# Via Altnautica admin API (Enterprise tier)
curl -X DELETE https://api.altnautica.com/v1/devices/{deviceId} \
  -H "Authorization: Bearer $ADMIN_API_KEY"
```

Self-hosted MQTT: remove the device's entry from the password file and reload Mosquitto.

```bash
# Remove device from password file
grep -v "^$DEVICE_ID:" passwords > passwords.tmp
mv passwords.tmp passwords
mosquitto_passwd -U passwords

# Signal Mosquitto to reload
kill -HUP $(pidof mosquitto)
```
