# Miyoubi Panel

A self-hosted Minecraft server management panel written in Rust. Supports Java and Bedrock (via GeyserMC), runs entirely on Docker Compose, and persists all data in SQLite.

---

## Quick start

```bash
# Build
cargo build --release

# Allow binding to port 80/443 without root (Linux)
sudo setcap 'cap_net_bind_service=+ep' ./target/release/minecraft-panel

# Run
./target/release/minecraft-panel
```

First run opens an interactive wizard. If you choose HTTPS it runs certbot, obtains a Let's Encrypt cert for your domain, and writes it to `config/tls.json`. Re-run with `--reconfigure` to redo the wizard. The wizard also creates the first admin account.

---

## Configuration

### Environment variables

| Variable      | Default                | Description              |
|---------------|------------------------|--------------------------|
| `PORT`        | `3000`                 | HTTP port                |
| `BIND_ADDR`   | `0.0.0.0`              | Address to bind to       |
| `CONFIG_DIR`  | `config`               | Root config directory    |
| `DOCKER_HOST` | `/var/run/docker.sock` | Docker daemon socket     |

### Config layout

```
config/
├── tls.json                   ← HTTPS settings (created by wizard)
├── panel.db                   ← SQLite database (users, logs, activity)
└── servers/
    └── my-server-a1b2c3d4/
        ├── server.json        ← Server metadata (name, port, opencl…)
        ├── docker-compose.yml ← Editable directly
        └── data/              ← Mounted into the container as /data
            └── gpu.sh         ← Written automatically when OpenCL is enabled
```

Every `docker-compose.yml` is standalone. You can `cd` into the server directory and run `docker compose up -d` without touching the panel.

---

## User roles

Three roles. Assign them in **Settings → Users**.

| Role         | What they can do |
|--------------|-----------------|
| **Admin**    | Everything — create and delete servers, manage all users, change any setting |
| **Operator** | Manage assigned servers — start/stop/restart, send console commands, browse and edit files, install/toggle mods, edit docker-compose config, manage backups. Cannot create/delete servers or manage users. |
| **Viewer**   | Read-only — server status, online players, activity log, death logs. No console, no file access, no controls. |

Admins and operators can be restricted to specific servers via the server-access checkboxes in the user list. Everyone can change their own password from the profile dropdown.

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
WorkingDirectory=/home/exer/minecraft-panel
ExecStart=/home/exer/minecraft-panel/target/release/minecraft-panel
Restart=on-failure
RestartSec=5
Environment=CONFIG_DIR=/home/exer/minecraft-panel/config

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp minecraft-panel.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now minecraft-panel
```

---

## Features

### Console & logs
Real-time Docker log streaming via SSE. Last 2 000 lines stored in SQLite — survive panel restarts. Mobile pre-loads history via REST so the console is never blank on first open.

### Activity log
Join, leave, and death events parsed from the console and stored persistently. Visible in real time; survives server restarts and log clears. Bedrock players detected automatically via GeyserMC `.` prefix.

### Death log
Structured per-player death history with timestamps and cause. Named villager deaths captured separately with villager name, death message, dimension, and coordinates.

### Mods
Browse, enable, disable, and remove mods. Search Modrinth and install with one click — the panel downloads JARs server-side.

### Backups
Enable per server in **Settings → Backup**. Appends an `itzg/mc-backup` sidecar to the compose file. Server keeps running. Archives in `config/servers/<id>/backups/`.

### OpenCL / GPU
Enable in **Settings → Resources**. Writes `gpu.sh`, sets the compose entrypoint, adds the NVIDIA device block. Requires `nvidia-container-runtime`.

### Public status pages
Every server gets `/s/<server-id>` — no login required. Shows status, uptime, CPU, RAM, players (with platform badges and death counts). Auto-refreshes every 10 s.

### Mobile UI
Touch-optimised layout at `/mobile`. Role restrictions apply identically.

---

## API

All routes require an authenticated session cookie unless noted. Role requirements are enforced server-side — returns 403 if the caller lacks the required role.

### Auth

| Method | Path               | Min role |
|--------|--------------------|----------|
| POST   | `/api/auth/login`  | —        |
| POST   | `/api/auth/logout` | any      |
| GET    | `/api/auth/me`     | any      |

### Servers

| Method | Path                            | Min role |
|--------|---------------------------------|----------|
| GET    | `/api/servers`                  | viewer   |
| POST   | `/api/servers`                  | admin    |
| DELETE | `/api/servers/:id`              | admin    |
| GET    | `/api/servers/:id/status`       | viewer   |
| GET    | `/api/servers/:id/stats`        | viewer   |
| POST   | `/api/servers/:id/start`        | operator |
| POST   | `/api/servers/:id/stop`         | operator |
| POST   | `/api/servers/:id/restart`      | operator |
| GET    | `/api/servers/:id/logs`         | viewer   |
| GET    | `/api/servers/:id/logs/history` | viewer   |
| POST   | `/api/servers/:id/logs/clear`   | operator |
| POST   | `/api/servers/:id/command`      | operator |
| GET    | `/api/servers/:id/players`      | viewer   |

### Activity & deaths

| Method | Path                                | Min role |
|--------|-------------------------------------|----------|
| GET    | `/api/servers/:id/activity`         | viewer   |
| POST   | `/api/servers/:id/activity/clear`   | operator |
| GET    | `/api/servers/:id/deaths`           | viewer   |
| GET    | `/api/servers/:id/deaths/villagers` | viewer   |
| GET    | `/api/servers/:id/deaths/:player`   | viewer   |

### Files

| Method | Path                                   | Min role |
|--------|----------------------------------------|----------|
| GET    | `/api/servers/:id/files?path=`         | operator |
| GET    | `/api/servers/:id/files/content?path=` | operator |
| POST   | `/api/servers/:id/files/write`         | operator |

### Mods

| Method | Path                            | Min role |
|--------|---------------------------------|----------|
| GET    | `/api/servers/:id/mods`         | operator |
| POST   | `/api/servers/:id/mods/install` | operator |
| POST   | `/api/servers/:id/mods/enable`  | operator |
| POST   | `/api/servers/:id/mods/disable` | operator |
| POST   | `/api/servers/:id/mods/remove`  | operator |

### Config / Backup / OpenCL

| Method | Path                      | Min role |
|--------|---------------------------|----------|
| GET    | `/api/servers/:id/config` | operator |
| POST   | `/api/servers/:id/config` | operator |
| GET    | `/api/servers/:id/backup` | operator |
| POST   | `/api/servers/:id/backup` | operator |
| POST   | `/api/servers/:id/opencl` | operator |

### Users

| Method | Path                            | Notes                               | Min role |
|--------|---------------------------------|-------------------------------------|----------|
| GET    | `/api/users`                    |                                     | admin    |
| POST   | `/api/users`                    | role: admin \| operator \| viewer   | admin    |
| PUT    | `/api/users/:username`          | Update role / password              | admin    |
| DELETE | `/api/users/:username`          | Cannot delete last admin            | admin    |
| POST   | `/api/users/:username/password` | Anyone can change own password      | any      |
| PUT    | `/api/users/:username/servers`  | Set allowed server IDs              | admin    |

### Public (no auth)

| Method | Path                      | Notes                    |
|--------|---------------------------|--------------------------|
| GET    | `/api/public/:id/status`  |                          |
| GET    | `/api/public/:id/stats`   |                          |
| GET    | `/api/public/:id/players` |                          |
| GET    | `/api/public/:id/deaths`  | player → death count map |
| GET    | `/s/:id`                  | Status page HTML         |

---

## License

MIT.
