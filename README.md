# HyprKVM
(noun) : yer a wizard, 'arry

### what?

Hyprland-native software KVM switch that integrates with workspace navigation.

## Features

- **Workspace-integrated switching**: Move past your last workspace to switch machines
- **Mouse edge switching**: Standard screen-edge triggers like Synergy/Barrier
- **Encrypted connections**: TLS with certificate pinning
- **Clipboard sharing**: Sync clipboard between machines
- **GUI and CLI**: Visual layout editor or config-file driven

## Status

🚧 **Early Development** 

## Building

### Prerequisites

- Rust 1.75+
- GTK4 development libraries (for future GUI)
- Hyprland

### Arch Linux / CachyOS

```bash
sudo pacman -S rust gtk4 libadwaita
```

### NixOS

```bash
nix develop  # When flake is available
```

### Build

```bash
cargo build --release
```

## Quick Start

1. Start the daemon:
   ```bash
   ./target/release/hyprkvm daemon
   ```

2. Configure neighbor machines in `~/.config/hyprkvm/hyprkvm.toml`:
   ```toml
   [machines]
   self_name = "my-laptop"

   [[machines.neighbors]]
   name = "desktop"
   direction = "right"
   address = "desktop.local:24850"
   ```

3. Optionally, install keybinding interceptors in your `hyprland.conf`:
   ```ini
   bind = SUPER, Left,  exec, ~/.config/hypr/scripts/hyprkvm-move.sh left
   bind = SUPER, Right, exec, ~/.config/hypr/scripts/hyprkvm-move.sh right
   bind = SUPER, Up,    exec, ~/.config/hypr/scripts/hyprkvm-move.sh up
   bind = SUPER, Down,  exec, ~/.config/hypr/scripts/hyprkvm-move.sh down
   ```

## Configuration

See [config/hyprkvm.example.toml](config/hyprkvm.example.toml) for all options.

## Architecture

See [docs/ROADMAP.md](docs/ROADMAP.md) for the full architecture and development plan.

## License

MIT License
