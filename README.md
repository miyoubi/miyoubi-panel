# Miyoubi Panel

A self-hosted Minecraft server management panel written in Rust. Runs each
server as an isolated Docker Compose project — no Kubernetes, no cloud
dependencies, just Docker.

## Features

- **Multi-server** — create, start, stop and restart any number of servers
- **Live console** — real-time log streaming via SSE (`docker attach`),
  handles custom entrypoints and OpenCL/GPU setups correctly
- **File browser** — browse, edit and save server files in the browser
- **Mod manager** — install mods directly from Modrinth, enable/disable/remove
- **Backup sidecar** — optional `itzg/mc-backup` container per server
  for scheduled backups with configurable retention
- **OpenCL / GPU** — optional NVIDIA GPU passthrough via `gpu.sh` entrypoint
- **User accounts** — admin and viewer roles, managed from the profile modal
- **Shareable status page** — public read-only page per server at `/s/:id`
  showing live status, CPU, memory and online players
- **HTTPS** — first-run wizard runs certbot and hot-reloads certs on renewal

---

## Quick start

```bash
# 1. Build
cargo build --release

# 2. Allow binding to port 80/443 without root (Linux)
sudo setcap 'cap_net_bind_service=+ep' ./target/release/minecraft-panel

# 3. Run (first run shows the HTTPS setup wizard)
./target/release/minecraft-panel
```

On first run the panel asks whether you want HTTPS. If you say yes it runs
`certbot` in standalone mode, obtains a Let's Encrypt certificate for your
domain, and saves the config to `config/tls.json`. Subsequent starts load the
cert automatically. Run with `--reconfigure` to redo the wizard.

---

## Configuration

### Environment variables

| Variable      | Default         | Purpose                                        |
|---------------|-----------------|------------------------------------------------|
| `PORT`        | `3000`          | Panel listen port                              |
| `BIND_ADDR`   | `127.0.0.1`     | Panel bind address                             |
| `CONFIG_DIR`  | `config`        | Root config directory (canonicalised on start) |
| `DOCKER_HOST` | `/var/run/docker.sock` | Docker daemon socket                  |

### Config directory layout

```
config/
├── tls.json                   ← HTTPS settings (created by first-run wizard)
├── users.json                 ← User accounts
└── servers/
    └── myserver-a1b2c3d4/
        ├── server.json        ← Server metadata (id, name, port, opencl…)
        ├── docker-compose.yml ← Fully editable; re-read on every start
        ├── data/              ← Bind-mounted into container as /data
        │   └── gpu.sh         ← GPU setup script (written when OpenCL enabled)
        ├── backups/           ← Backup archives (when backup sidecar enabled)
        └── console.log        ← Persistent console history (last 2000 lines)
```

Each `docker-compose.yml` is a standalone file — you can edit it directly and
run `docker compose up -d` from that directory without going through the panel.

---

## User roles

| Role     | Can do                                                     |
|----------|------------------------------------------------------------|
| `admin`  | Everything — create/delete servers, files, mods, users     |
| `viewer` | Read status cards, players list; use Start/Stop/Restart    |

Manage users from the **profile modal** (click your name in the bottom-left
of the sidebar). Admins see a full user list and an "Add User" form there.
All users can change their own password from the same modal.

---

## API reference

All API routes require an authenticated session cookie except the `/api/public`
routes (which are intentionally unauthenticated and read-only).

### Auth

| Method | Path         | Body                                   |
|--------|--------------|----------------------------------------|
| POST   | `/api/login` | `{ "username": "…", "password": "…" }` |
| POST   | `/api/logout`|                                        |

### Servers

| Method | Path                              | Description                     |
|--------|-----------------------------------|---------------------------------|
| GET    | `/api/servers`                    | List all servers                |
| POST   | `/api/servers`                    | Create a server                 |
| DELETE | `/api/servers/:id`                | Delete server and data          |
| GET    | `/api/servers/:id/status`         | Container status (fast, no CPU/mem) |
| GET    | `/api/servers/:id/stats`          | CPU + memory (separate, ~1s)    |
| POST   | `/api/servers/:id/start`          | `docker compose up -d`          |
| POST   | `/api/servers/:id/stop`           | `docker compose stop`           |
| POST   | `/api/servers/:id/restart`        | `docker compose restart`        |
| GET    | `/api/servers/:id/logs`           | SSE console stream              |
| POST   | `/api/servers/:id/command`        | Send RCON command               |
| GET    | `/api/servers/:id/players`        | Online players                  |

### Files

| Method | Path                                   | Description        |
|--------|----------------------------------------|--------------------|
| GET    | `/api/servers/:id/files?path=`         | List directory     |
| GET    | `/api/servers/:id/files/content?path=` | Read file          |
| POST   | `/api/servers/:id/files/write`         | Write file         |

### Mods

| Method | Path                              | Description                        |
|--------|-----------------------------------|------------------------------------|
| GET    | `/api/servers/:id/mods`           | List installed mods                |
| POST   | `/api/servers/:id/mods/install`   | Install from Modrinth              |
| POST   | `/api/servers/:id/mods/enable`    | Enable a disabled mod              |
| POST   | `/api/servers/:id/mods/disable`   | Disable a mod                      |
| POST   | `/api/servers/:id/mods/remove`    | Delete a mod                       |
| GET    | `/api/modrinth/search`            | Proxy Modrinth search              |
| GET    | `/api/modrinth/versions`          | Modrinth version list for a project|

### Config, Backup, OpenCL

| Method | Path                           | Description                      |
|--------|--------------------------------|----------------------------------|
| GET    | `/api/servers/:id/config`      | Read docker-compose.yml          |
| POST   | `/api/servers/:id/config`      | Write docker-compose.yml         |
| GET    | `/api/servers/:id/backup`      | Backup config + file list        |
| POST   | `/api/servers/:id/backup`      | Enable/update/disable backup     |
| POST   | `/api/servers/:id/opencl`      | Toggle OpenCL/GPU support        |

### Users

| Method | Path                                  | Description                     |
|--------|---------------------------------------|---------------------------------|
| GET    | `/api/users`                          | List users (admin only)         |
| POST   | `/api/users`                          | Create user (admin only)        |
| PUT    | `/api/users/:username`                | Update user (admin only)        |
| DELETE | `/api/users/:username`                | Delete user (admin only)        |
| POST   | `/api/users/:username/password`       | Change own password (any user)  |

### Public (no auth)

| Method | Path                          | Description                          |
|--------|-------------------------------|--------------------------------------|
| GET    | `/api/public/:id/status`      | Running state, uptime                |
| GET    | `/api/public/:id/stats`       | CPU % and memory usage               |
| GET    | `/api/public/:id/players`     | Online player list                   |
| GET    | `/s/:id`                      | Shareable status page (HTML)         |

---

## OpenCL / GPU support

Enable per-server in **Settings → Resources → GPU / OpenCL Support**, or
tick the toggle in the server creation wizard.

When enabled the panel:

1. Writes `gpu.sh` into the server's `data/` directory (installs
   `ocl-icd-libopencl1` and sets up the NVIDIA OpenCL ICD)
2. Sets the compose entrypoint to `bash /data/gpu.sh && exec /start`
   (`exec` replaces bash so Java is PID 1 — required for console log capture)
3. Adds `NVIDIA_VISIBLE_DEVICES: all` and the `deploy.resources.devices` block

Optionally also enable **`runtime: nvidia`** if you have
`nvidia-container-runtime` installed (`docker info | grep -i runtime`).
Without it, Docker uses CDI/device-plugin mode which works on most setups.

---

## Backup sidecar

Enable per-server in **Settings → Backup**. When enabled the panel appends an
`itzg/mc-backup` service to the server's `docker-compose.yml` and runs
`docker compose up -d`. The Minecraft server is never stopped.

Backups are written as `.tar.gz` archives to `config/servers/<name>/backups/`.
Configurable options: cron schedule, retention count, RCON password.

---

## Shareable status page

Every server has a public status page at:

```
https://your-panel-domain/s/<server-id>
```

It shows online/offline status, uptime, CPU usage, memory usage and the online
player list. It auto-refreshes every 10 seconds and requires no login.
Copy the link from the **Overview** tab inside the panel.

---

## Systemd service

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
