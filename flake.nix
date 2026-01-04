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

          # For GTK4 GUI (future)
          gtk4
          libadwaita

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

        # Package (for later)
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "hyprkvm";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = buildDeps ++ [ pkgs.wrapGAppsHook4 ];
          buildInputs = libDeps;

          meta = with pkgs.lib; {
            description = "Hyprland-native software KVM switch";
            homepage = "https://github.com/tenseleyFlow/hyprKVM";
            license = licenses.mit;
            platforms = platforms.linux;
          };
        };
      }
    );
}
