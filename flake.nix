{
  description = "Clipboard policy and clip-api daemon for Shelllist";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # Co-development: one live sibling framework; no per-daemon revision pins.
  # Use ../daemon-framework/tools/local-build for Nix builds/checks.
  inputs.daemonFramework = {
    url = "git+file:../daemon-framework";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      daemonFramework,
    }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        system: pkgs:
        let
          ringboard = import ./packaging/ringboard.nix { inherit pkgs; };
          satty = pkgs.satty.overrideAttrs (old: {
            patches = (old.patches or [ ]) ++ [ ./packaging/satty-toolbar-layout.patch ];
          });
          clipDaemon = daemonFramework.lib.buildRustPackage pkgs {
            pname = "clip-daemon";
            version = "0.1.0";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./test_support
                ./examples
                ./vendor/shelllist-search
              ];
            };
            postUnpack = ''
              cp -R --no-preserve=mode ${daemonFramework.lib.daemonSource pkgs} "$(dirname "$sourceRoot")/daemon-framework"
            '';
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.pkg-config
            ];
            buildInputs = [ pkgs.dbus ];
            nativeCheckInputs = [ pkgs.dbus ];
            # Include private heavy framing cases; native Quickshell acceptance
            # remains excluded by its non-default Cargo feature.
            checkFlags = [ "--include-ignored" ];
            strictDeps = true;
            # Only the pinned Ringboard crates need core_io_borrowed_buf.
            RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
            postInstall = ''
              for unit in ${./packaging/systemd}/*.service; do
                install -Dm644 "$unit" "$out/share/systemd/user/$(basename "$unit")"
              done
              substituteInPlace $out/share/systemd/user/*.service \
                --replace-fail @out@ $out
              substituteInPlace $out/share/systemd/user/ringboard-server.service \
                --replace-fail @ringboard@ ${ringboard}
              install -Dm644 ${./packaging/dbus/org.laufan.ClipDaemon.service} \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service
              install -Dm644 ${./integrations/yazi/yank-to-clip-daemon.yazi/main.lua} \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua
              substituteInPlace \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua \
                --replace-fail @out@ $out
            '';
            postFixup = ''
              wrapProgram $out/bin/clip-daemon \
                --prefix PATH : ${
                  pkgs.lib.makeBinPath [
                    pkgs.coreutils
                    pkgs.grim
                    pkgs.slurp
                    pkgs.hyprland
                    pkgs.libnotify
                    satty
                    pkgs.systemd
                    pkgs.xdg-utils
                  ]
                }
            '';
            meta = {
              description = "Wayland clipboard policy and clip-api daemon for Shelllist";
              mainProgram = "clip-daemon";
              platforms = systems;
              license = pkgs.lib.licenses.mit;
            };
          };
        in
        {
          default = clipDaemon;
          inherit ringboard;
          imageEditor = satty;
          ringboardQualification = pkgs.writeShellApplication {
            name = "clip-daemon-ringboard-qualification";
            runtimeInputs = [
              pkgs.jq
              self.packages.${system}.ringboard
              pkgs.wayland-utils
            ];
            text = builtins.readFile ./scripts/qualify-ringboard.sh;
          };
        }
      );

      apps = forAllSystems (
        system: pkgs: {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/clip-daemon";
          };
          qualify = {
            type = "app";
            program = "${
              self.packages.${system}.ringboardQualification
            }/bin/clip-daemon-ringboard-qualification";
          };
        }
      );

      checks = forAllSystems (
        system: pkgs:
        let
          smokeSource = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./scripts/package-smoke.py
              ./tests/test_package_smoke.py
              ./test_support/package_smoke_service.py
              ./packaging/systemd
              ./packaging/dbus
              ./src/daemon.rs
            ];
          };
          smokePython = pkgs.python3.withPackages (ps: [ ps.pygobject3 ]);
        in
        {
          default = self.packages.${system}.default;
          # Only inert fixtures may activate on this explicitly private bus.
          # The actual package is inspected statically; its main/engine never run.
          packageSmoke = pkgs.runCommand "clip-daemon-private-package-smoke" {
            nativeBuildInputs = [
              smokePython
              pkgs.dbus
              pkgs.systemd
              pkgs.coreutils
            ];
          } ''
            export HOME="$TMPDIR/home" XDG_RUNTIME_DIR="$TMPDIR/run"
            export XDG_CONFIG_HOME="$TMPDIR/config" XDG_DATA_HOME="$TMPDIR/data"
            export XDG_STATE_HOME="$TMPDIR/state" XDG_CACHE_HOME="$TMPDIR/cache"
            export XDG_CONFIG_DIRS="$TMPDIR/empty-config" XDG_DATA_DIRS="$TMPDIR/empty-data"
            mkdir -m 700 -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" \
              "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_DIRS" "$XDG_DATA_DIRS"
            unset DISPLAY WAYLAND_SOCKET DBUS_STARTER_ADDRESS DBUS_STARTER_BUS_TYPE
            export WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/no-wayland"
            export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/no-session-bus"
            export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/no-system-bus"
            export CLIP_PACKAGE_NATIVE=1 PYTHONDONTWRITEBYTECODE=1
            timeout --kill-after=5 120 python3 ${smokeSource}/tests/test_package_smoke.py
            python3 ${smokeSource}/scripts/package-smoke.py \
              ${self.packages.${system}.default} --wiring-only
            touch "$out"
          '';
        }
      );

      devShells = forAllSystems (
        system: pkgs: {
          default = pkgs.mkShell {
            packages =
              (daemonFramework.lib.toolchain pkgs).packages
              ++ (with pkgs; [
                cargo-audit
                cargo-machete
                cargo-llvm-cov
                dbus
                ghostty
                git
                gobject-introspection
                grim
                gtk3
                hyprland
                jq
                just
                llvmPackages.llvm
                pkg-config
                (python3.withPackages (ps: [ ps.pygobject3 ]))
                quickshell
                self.packages.${system}.ringboard
                rust-analyzer
                self.packages.${system}.imageEditor
                wayland-utils
                wl-clipboard
              ]);
            GI_TYPELIB_PATH = pkgs.lib.makeSearchPath "lib/girepository-1.0" [
              pkgs.gtk3
              pkgs.glib
              pkgs.pango
              pkgs.gdk-pixbuf
              pkgs.at-spi2-core
              pkgs.harfbuzz
            ];
            LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
            LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";
            RUST_BACKTRACE = "1";
            RUST_LOG = "clip_daemon=debug";
            RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
          };
        }
      );

      formatter = forAllSystems (system: pkgs: pkgs.nixpkgs-fmt);
    };
}
