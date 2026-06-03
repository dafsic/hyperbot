# Hyperbot deployment (Ubuntu 24.04, systemd + encrypted credentials)

The bot reads three secrets — `private_key`, `database_url`, and (optionally)
`account_address` — from **systemd encrypted credentials**. systemd decrypts
them at start into a per-service tmpfs (`$CREDENTIALS_DIRECTORY`) that only the
running process can read; they are never stored on disk in plaintext and are not
exposed to other users or in the unit file.

The bot still falls back to environment variables (`HYPERBOT_PRIVATE_KEY`,
`DATABASE_URL`, `HYPERBOT_ACCOUNT_ADDRESS`) when no credential is present, so
local development is unchanged.

## 1. Build and install

```bash
cargo build --release

sudo useradd --system --no-create-home --shell /usr/sbin/nologin hyperbot
sudo mkdir -p /opt/hyperbot /etc/hyperbot/credentials
sudo cp target/release/hyperbot /opt/hyperbot/
sudo cp config.toml /opt/hyperbot/        # optional; built-in defaults work too
sudo chown -R hyperbot:hyperbot /opt/hyperbot
```

## 2. Create the encrypted credentials

`systemd-creds encrypt` ties the ciphertext to this host (uses the host key, and
the TPM when available), so the `.cred` files only decrypt on THIS machine for
THIS service. Run as root:

```bash
# private key (paste the hex, then Ctrl-D):
sudo systemd-creds encrypt --name=private_key - /etc/hyperbot/credentials/private_key.cred

# database url:
printf 'postgres://hyperbot:password@localhost:5432/hyperbot' \
  | sudo systemd-creds encrypt --name=database_url - /etc/hyperbot/credentials/database_url.cred

# optional account address (skip if using config.toml / defaults):
printf '0xYourMainAccount' \
  | sudo systemd-creds encrypt --name=account_address - /etc/hyperbot/credentials/account_address.cred

sudo chmod 600 /etc/hyperbot/credentials/*.cred
sudo chown root:root /etc/hyperbot/credentials/*.cred
```

> The `--name=` MUST match the credential id referenced in the unit
> (`LoadCredentialEncrypted=<id>:<path>`); systemd verifies it on decrypt.

If you are NOT using `account_address` as a credential, delete its
`LoadCredentialEncrypted=` line from `hyperbot.service`.

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
