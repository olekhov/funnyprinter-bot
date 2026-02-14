# Deployment

## 1) Host service: printerd (systemd)

Install binary on host (Raspberry Pi):

```bash
cd /opt/printerbot
cargo build --release -p printerd
sudo install -Dm755 target/release/printerd /opt/printerbot/bin/printerd
```

Install unit + env:

```bash
sudo install -Dm644 deploy/systemd/printerd.service /etc/systemd/system/printerd.service
sudo install -Dm644 deploy/systemd/printerd.env.example /etc/printerbot/printerd.env
sudo $EDITOR /etc/printerbot/printerd.env
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now printerd
sudo systemctl status printerd --no-pager
journalctl -u printerd -f
```

## 2) Containers: single image, two services

`docker-compose` builds one shared image (`printerbot/app:latest`) that contains both binaries:

- `/usr/local/bin/ai-service`
- `/usr/local/bin/telegram-bot`

Then runs them as separate containers/services with different `command`.

Prepare config files:

```bash
cd /opt/printerbot/deploy
cp .env.example .env
cp bot-config.docker.example.toml bot-config.toml
$EDITOR .env
$EDITOR bot-config.toml
mkdir -p data
```

Run:

```bash
docker compose -f docker-compose.yml up -d --build
```

Logs:

```bash
docker compose -f docker-compose.yml logs -f telegram-bot
docker compose -f docker-compose.yml logs -f ai-service
```
