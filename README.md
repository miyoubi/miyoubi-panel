# Miyoubi Panel

A self-hosted Minecraft server panel written in Rust.

---

## Quick start

```bash
# Build
cargo build --release

# Allow binding to 80/443 without root (Linux)
sudo setcap 'cap_net_bind_service=+ep' ./target/release/minecraft-panel

# Run
./target/release/minecraft-panel
```

First run will ask if you want HTTPS. Say yes and it runs certbot, grabs a Let's Encrypt cert for your domain, and saves it to `config/tls.json`. Run with `--reconfigure` if you need to redo the wizard later.

---

## Configuration

### Environment variables

| Variable      | Default                  | What it does                                   |
|---------------|--------------------------|------------------------------------------------|
| `PORT`        | `3000`                   | Port the panel listens on                      |
| `BIND_ADDR`   | `127.0.0.1`              | Address to bind to                             |
| `CONFIG_DIR`  | `config`                 | Root config directory                          |
| `DOCKER_HOST` | `/var/run/docker.sock`   | Docker daemon socket                           |

### Config layout

```
config/
├── tls.json                   ← HTTPS config (created by first-run wizard)
├── users.json                 ← User accounts
└── servers/
    └── myserver-a1b2c3d4/
        ├── server.json        ← Server metadata (id, name, port, opencl...)
        ├── docker-compose.yml ← Edit this directly if you want
        ├── data/              ← Mounted into the container as /data
        │   └── gpu.sh         ← Written automatically when OpenCL is enabled
        ├── backups/           ← Backup archives (when backup sidecar is on)
        └── console.log        ← Last 2000 lines of console output
```

Every `docker-compose.yml` is a normal standalone file. You can `cd` into the server directory and run `docker compose up -d` without touching the panel if you want.

---

## User roles

| Role     | Access                                                         |
|----------|----------------------------------------------------------------|
| `admin`  | Everything — create/delete servers, files, mods, users         |
| `viewer` | Read-only status and player info, plus Start/Stop/Restart      |

Manage users from the profile modal (click your name at the bottom of the sidebar). Admins see the full user list and can add/remove accounts. Everyone can change their own password from there too.

---

## Systemd

```ini
[Unit]
Description=Miyoubi Minecraft Panel
After=network.target docker.service
Requires=docker.service

[Service]
Type=simple
User=exer
WorkingDirectory=/home/exer/rust-panel
ExecStart=/home/exer/rust-panel/target/release/minecraft-panel
Restart=on-failure
RestartSec=5
Environment=CONFIG_DIR=/home/exer/rust-panel/config

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp minecraft-panel.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now minecraft-panel
```

---

## API

All routes require an authenticated session cookie except the `/api/public` ones.

### Auth

| Method | Path               | Body                                    |
|--------|--------------------|-----------------------------------------|
| POST   | `/api/auth/login`  | `{ "username": "...", "password": "..." }` |
| POST   | `/api/auth/logout` |                                         |
| GET    | `/api/auth/me`     |                                         |

### Servers

| Method | Path                           | Notes                              |
|--------|--------------------------------|------------------------------------|
| GET    | `/api/servers`                 | List all                           |
| POST   | `/api/servers`                 | Create                             |
| DELETE | `/api/servers/:id`             | Delete server + all data           |
| GET    | `/api/servers/:id/status`      | Fast status, no CPU/mem            |
| GET    | `/api/servers/:id/stats`       | CPU + memory (~1s Docker call)     |
| POST   | `/api/servers/:id/start`       |                                    |
| POST   | `/api/servers/:id/stop`        |                                    |
| POST   | `/api/servers/:id/restart`     |                                    |
| GET    | `/api/servers/:id/logs`        | SSE stream                         |
| POST   | `/api/servers/:id/command`     | RCON command                       |
| GET    | `/api/servers/:id/players`     | Online players                     |

### Files

| Method | Path                                    |
|--------|-----------------------------------------|
| GET    | `/api/servers/:id/files?path=`          |
| GET    | `/api/servers/:id/files/content?path=`  |
| POST   | `/api/servers/:id/files/write`          |

### Mods

| Method | Path                              |
|--------|-----------------------------------|
| GET    | `/api/servers/:id/mods`           |
| POST   | `/api/servers/:id/mods/install`   |
| POST   | `/api/servers/:id/mods/enable`    |
| POST   | `/api/servers/:id/mods/disable`   |
| POST   | `/api/servers/:id/mods/remove`    |

### Config / Backup / OpenCL

| Method | Path                          |
|--------|-------------------------------|
| GET    | `/api/servers/:id/config`     |
| POST   | `/api/servers/:id/config`     |
| GET    | `/api/servers/:id/backup`     |
| POST   | `/api/servers/:id/backup`     |
| POST   | `/api/servers/:id/opencl`     |

### Users (admin only except password change)

| Method | Path                                 |
|--------|--------------------------------------|
| GET    | `/api/users`                         |
| POST   | `/api/users`                         |
| PUT    | `/api/users/:username`               |
| DELETE | `/api/users/:username`               |
| POST   | `/api/users/:username/password`      |
| PUT    | `/api/users/:username/servers`       |

### Public (no auth required)

| Method | Path                      |
|--------|---------------------------|
| GET    | `/api/public/:id/status`  |
| GET    | `/api/public/:id/stats`   |
| GET    | `/api/public/:id/players` |
| GET    | `/s/:id`                  |

---

## OpenCL / GPU

Enable per server in **Settings → Resources → GPU / OpenCL Support**, or tick the toggle in the creation wizard.

When enabled, the panel:

1. Writes `gpu.sh` into the server's `data/` folder — installs `ocl-icd-libopencl1` and sets up the NVIDIA OpenCL ICD
2. Sets the compose entrypoint to `bash /data/gpu.sh && exec /start` (the `exec` makes Java PID 1, which is required for log capture to work)
3. Adds `NVIDIA_VISIBLE_DEVICES: all` and the `deploy.resources.devices` block

If you have `nvidia-container-runtime` installed you can also flip on `runtime: nvidia`. Without it, CDI/device-plugin mode works fine on most setups.

---

## Backups

Enable per server in **Settings → Backup**. The panel appends an `itzg/mc-backup` sidecar service to the server's `docker-compose.yml` and brings it up. The Minecraft server keeps running the whole time.

Archives are written as `.tar.gz` to `config/servers/<n>/backups/`. You can configure the cron schedule, how many backups to keep, and the RCON password from the same settings panel.

---

## Status pages

Every server gets a public status page at `/s/<server-id>`. It shows online/offline state, uptime, CPU, memory and the player list, auto-refreshes every 10 seconds, and needs no login. Grab the link from the Share button in the server panel header.

---

## License

MIT.
