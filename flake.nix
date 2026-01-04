{
  description = "HyprKVM - Hyprland-native software KVM switch";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Build dependencies
        buildDeps = with pkgs; [
          pkg-config
        ];

        # Runtime/library dependencies
        libDeps = with pkgs; [
          # Wayland
          wayland
          wayland-protocols

          # For smithay-client-toolkit
          libxkbcommon

          # TLS
          openssl
        ];

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };
      in
      {
        # Development shell
        devShells.default = pkgs.mkShell {
          buildInputs = buildDeps ++ libDeps ++ [
            rustToolchain
          ];

          # Set up pkg-config paths
          PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" libDeps;

          # For wayland-scanner
          WAYLAND_PROTOCOLS = "${pkgs.wayland-protocols}/share/wayland-protocols";

          shellHook = ''
            echo "HyprKVM development shell"
            echo "Rust: $(rustc --version)"
            echo ""
            echo "Run 'cargo build' to compile"
          '';
        };

        # Package
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "hyprkvm";
          version = "0.5.1";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = buildDeps;
          buildInputs = libDeps;

          # Builds both hyprkvm (daemon) and hyprkvm-ctl (CLI)
          # buildRustPackage automatically installs all workspace binaries

          meta = with pkgs.lib; {
            description = "Hyprland-native software KVM switch";
            longDescription = ''
              HyprKVM enables seamless keyboard/mouse control transfer between
              Linux machines running Hyprland. Move past your last workspace
              to switch to another machine.
            '';
            homepage = "https://github.com/tenseleyFlow/hyprKVM";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "hyprkvm";
          };
        };
      }
    );
}
