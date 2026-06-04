# Hyperbot deployment (Ubuntu 24.04, systemd + encrypted credentials)

## 1. Build and install

```bash
cargo build --release

sudo useradd --system --no-create-home --shell /usr/sbin/nologin hyperbot
sudo mkdir -p /opt/hyperbot /etc/hyperbot/credentials
sudo cp target/release/hyperbot /opt/hyperbot/
sudo cp config.toml /opt/hyperbot/        # optional; built-in defaults work too
sudo chown -R hyperbot:hyperbot /opt/hyperbot
```

## 3. Install and start the service

```bash
sudo cp deploy/hyperbot.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now hyperbot
```

## 4. Operate

```bash
systemctl status hyperbot
sudo systemctl stop hyperbot        # graceful (SIGTERM)
sudo systemctl restart hyperbot
journalctl -u hyperbot -f           # follow logs
```

## Rotating a secret

Re-encrypt and restart — no rebuild needed:

```bash
printf 'NEW_SECRET' | sudo systemd-creds encrypt --name=private_key - /etc/hyperbot/credentials/private_key.cred
sudo systemctl restart hyperbot
```

## Security notes

- A non-root user cannot read the `.cred` files (mode 600, root-owned) nor the
  decrypted credentials (per-service tmpfs). They can `systemctl stop` the
  service only if granted; they cannot extract the key.
- Root can always read a running process's memory, so no on-host scheme defends
  against a hostile root. Encrypted credentials defend against everyone below
  root and against offline disk theft.
