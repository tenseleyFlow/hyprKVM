# HyprKVM

Hyprland-native software KVM switch that integrates with workspace navigation.

## Features

- **Workspace-integrated switching**: Move past your last workspace to switch machines
- **Mouse edge switching**: Standard screen-edge triggers like Synergy/Barrier
- **Hyprland keybind switching**: Trigger switches using your movefocus Hyprland binds
- **TLS encryption**: Secure connections with TOFU (Trust On First Use) certificate pinning
- **Clipboard sharing**: Automatic clipboard sync between machines

## Installation

### Arch Linux (AUR)

```bash
yay -S hyprkvm
# or
paru -S hyprkvm
```

### NixOS / Nix

Add to your flake inputs:

```nix
{
  inputs.hyprkvm.url = "github:tenseleyFlow/hyprKVM";
}
```

Then add to your packages:

```nix
environment.systemPackages = [
  inputs.hyprkvm.packages.${pkgs.system}.default
];
```

Or use the standalone flake directly:

```bash
nix run github:tenseleyFlow/hyprKVM -- daemon
```

### Build from Source

```bash
# Clone
git clone https://github.com/tenseleyFlow/hyprKVM.git
cd hyprKVM

# NixOS
nix develop
cargo build --release

# Arch Linux / CachyOS
sudo pacman -S rust wayland libxkbcommon openssl
cargo build --release

# Binaries output to target/release/hyprkvm and target/release/hyprkvm-ctl
```

## Quick Start

### 1. Configure

Create `~/.config/hyprkvm/hyprkvm.toml`:

```toml
[machines]
self_name = "laptop"

[[machines.neighbors]]
name = "desktop"
direction = "right"
address = "192.168.1.100:24850"
```

Each machine needs its own config pointing to its neighbors.

### 2. Start the Daemon

```bash
# If installed via package manager
hyprkvm daemon

# If built from source
./target/release/hyprkvm daemon
```

### 3. First Connection

On first connection, you'll be prompted to trust the remote machine's certificate fingerprint. This is stored in `~/.config/hyprkvm/known_hosts.toml` for future connections.

### 4. Usage

- **Mouse**: Move cursor to screen edge with a configured neighbor
- **Escape**: Press Scroll Lock (or triple-tap Shift) to return control

## CLI Commands

```bash
# Start the daemon
hyprkvm daemon

# View logs
hyprkvm-ctl logs

# Check status
hyprkvm-ctl status
```

## Configuration

### Neighbor Machines

```toml
[[machines.neighbors]]
name = "desktop"
direction = "right"      # left, right, up, down
address = "192.168.1.100:24850"
# fingerprint = "SHA256:..."  # Optional: pre-trust certificate
```

### Network & TLS

```toml
[network]
listen_port = 24850
bind_address = "0.0.0.0"
connect_timeout_secs = 5
ping_interval_secs = 5
ping_timeout_secs = 10

[network.tls]
# Auto-generated if missing
cert_path = "~/.config/hyprkvm/cert.pem"
key_path = "~/.config/hyprkvm/key.pem"
```

### Escape Hotkey

```toml
[input.escape_hotkey]
key = "scroll_lock"
modifiers = []  # e.g., ["super"] for Super+ScrollLock

[input]
triple_tap_enabled = true
triple_tap_key = "shift"
triple_tap_window_ms = 500
```

### Clipboard

```toml
[clipboard]
enabled = true
sync_on_enter = true
sync_on_leave = true
sync_text = true
sync_images = true
max_size = 10485760  # 10MB
```

### Logging

```toml
[logging]
level = "info"  # error, warn, info, debug, trace
```

See [config/hyprkvm.example.toml](config/hyprkvm.example.toml) for all options.

## Hyprland Keybind Integration (Optional)

Intercept workspace movement to trigger machine switching:

```ini
# ~/.config/hypr/hyprland.conf
bind = SUPER, Left,  exec, hyprkvm-ctl move left
bind = SUPER, Right, exec, hyprkvm-ctl move right
bind = SUPER, Up,    exec, hyprkvm-ctl move up
bind = SUPER, Down,  exec, hyprkvm-ctl move down
```

## Firewall

Open port 24850 (or your configured port):

```bash
# UFW
sudo ufw allow 24850/tcp

# firewalld
sudo firewall-cmd --add-port=24850/tcp --permanent
sudo firewall-cmd --reload
```

## License

MIT License
