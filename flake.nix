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
    let
      # System-independent outputs
      systemIndependent = {
        # Home-manager module for easy service management
        homeManagerModules.default = { config, lib, pkgs, ... }:
          let
            cfg = config.services.hyprkvm;
          in {
            options.services.hyprkvm = {
              enable = lib.mkEnableOption "HyprKVM daemon";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.system}.default;
                defaultText = lib.literalExpression "pkgs.hyprkvm";
                description = "The HyprKVM package to use.";
              };

              settings = lib.mkOption {
                type = lib.types.attrs;
                default = {};
                description = ''
                  Configuration for HyprKVM. Will be written to
                  ~/.config/hyprkvm/hyprkvm.toml
                '';
                example = lib.literalExpression ''
                  {
                    machines = {
                      self_name = "my-machine";
                      neighbors = [
                        { name = "other-machine"; direction = "right"; address = "192.168.1.100:24850"; }
                      ];
                    };
                    network = {
                      listen_port = 24850;
                    };
                  }
                '';
              };

              extraFlags = lib.mkOption {
                type = lib.types.listOf lib.types.str;
                default = [];
                description = "Extra command-line flags to pass to the daemon.";
              };
            };

            config = lib.mkIf cfg.enable {
              # Install the package
              home.packages = [ cfg.package ];

              # Generate config file if settings provided
              xdg.configFile."hyprkvm/hyprkvm.toml" = lib.mkIf (cfg.settings != {}) {
                source = (pkgs.formats.toml {}).generate "hyprkvm.toml" cfg.settings;
              };

              # Systemd user service
              systemd.user.services.hyprkvm = {
                Unit = {
                  Description = "HyprKVM Daemon - Hyprland-native software KVM switch";
                  After = [ "graphical-session.target" ];
                  PartOf = [ "graphical-session.target" ];
                };

                Service = {
                  Type = "simple";
                  ExecStart = "${cfg.package}/bin/hyprkvm daemon ${lib.concatStringsSep " " cfg.extraFlags}";
                  Restart = "on-failure";
                  RestartSec = 1;
                  # Exit code 75 triggers restart for direction changes
                  RestartForceExitStatus = [ 75 ];
                  Environment = [ "WAYLAND_DISPLAY=wayland-1" ];
                };

                Install = {
                  WantedBy = [ "graphical-session.target" ];
                };
              };
            };
          };

        # Alias for backwards compatibility
        homeManagerModules.hyprkvm = self.homeManagerModules.default;
      };

      # System-specific outputs (packages, devShells)
      systemSpecific = flake-utils.lib.eachDefaultSystem (system:
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

            # GUI (Iced) dependencies
            vulkan-loader
            libGL
            xorg.libX11
            xorg.libXcursor
            xorg.libXrandr
            xorg.libXi
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

            # Graphics library paths for GUI
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libDeps;

            shellHook = ''
              echo "HyprKVM development shell"
              echo "Rust: $(rustc --version)"
              echo ""
              echo "Run 'cargo build' to compile"
            '';
          };

          # Package (without GUI for smaller binary)
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

          # Package with GUI support
          packages.gui = pkgs.rustPlatform.buildRustPackage {
            pname = "hyprkvm";
            version = "0.5.1";
            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = buildDeps ++ [ pkgs.makeWrapper ];
            buildInputs = libDeps;

            # Build with GUI feature
            buildFeatures = [ "gui" ];

            # Wrap binary to include graphics library paths
            postInstall = ''
              wrapProgram $out/bin/hyprkvm \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath libDeps}
            '';

            meta = with pkgs.lib; {
              description = "Hyprland-native software KVM switch (with GUI)";
              longDescription = ''
                HyprKVM enables seamless keyboard/mouse control transfer between
                Linux machines running Hyprland. Move past your last workspace
                to switch to another machine.

                This package includes the GUI configuration tool.
              '';
              homepage = "https://github.com/tenseleyFlow/hyprKVM";
              license = licenses.mit;
              platforms = platforms.linux;
              mainProgram = "hyprkvm";
            };
          };
        }
      );
    in
    # Merge system-independent and system-specific outputs
    systemIndependent // systemSpecific;
}
