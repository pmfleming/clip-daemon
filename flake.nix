{
  description = "Clipboard policy and clip-api daemon for Shelllist";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.daemonFramework = {
    url = "git+file:../daemon-framework?ref=main";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, daemonFramework }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (system: pkgs:
        let
          satty = pkgs.satty.overrideAttrs (old: {
            patches = (old.patches or [ ]) ++ [ ./packaging/satty-toolbar-layout.patch ];
          });
          clipDaemon = pkgs.rustPlatform.buildRustPackage {
            pname = "clip-daemon";
            version = "0.1.0";
            src = ./.;
            postUnpack = ''
              cp -R --no-preserve=mode ${daemonFramework} "$(dirname "$sourceRoot")/daemon-framework"
            '';
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
            buildInputs = [ pkgs.dbus ];
            strictDeps = true;
            # Only the pinned Ringboard crates need core_io_borrowed_buf.
            RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
            postInstall = ''
              install -Dm644 ${./packaging/systemd/clip-daemon.service} $out/share/systemd/user/clip-daemon.service
              install -Dm644 ${./packaging/dbus/org.laufan.ClipDaemon.service} \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service
              install -Dm644 ${./integrations/yazi/yank-to-clip-daemon.yazi/main.lua} \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua
              substituteInPlace \
                $out/share/systemd/user/clip-daemon.service \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua \
                --replace-fail @out@ $out
            '';
            postFixup = ''
              wrapProgram $out/bin/clip-daemon \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.coreutils pkgs.grim pkgs.hyprland pkgs.libnotify satty pkgs.systemd pkgs.xdg-utils ]}
            '';
            meta = {
              description = "Wayland clipboard policy and clip-api daemon for Shelllist";
              mainProgram = "clip-daemon";
              platforms = pkgs.lib.platforms.linux;
            };
          };
        in {
          default = clipDaemon;
          ringboard = import ./packaging/ringboard.nix { inherit pkgs; };
          imageEditor = satty;
          ringboardQualification = pkgs.writeShellApplication {
            name = "clip-daemon-ringboard-qualification";
            runtimeInputs = [ pkgs.jq self.packages.${system}.ringboard pkgs.wayland-utils ];
            text = builtins.readFile ./scripts/qualify-ringboard.sh;
          };
        });

      apps = forAllSystems (system: pkgs: {
        default = { type = "app"; program = "${self.packages.${system}.default}/bin/clip-daemon"; };
        qualify = { type = "app"; program = "${self.packages.${system}.ringboardQualification}/bin/clip-daemon-ringboard-qualification"; };
      });

      checks = forAllSystems (system: pkgs: {
        default = self.packages.${system}.default;
      });

      devShells = forAllSystems (system: pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo cargo-audit cargo-machete cargo-llvm-cov clippy dbus gobject-introspection grim gtk3 hyprland jq just llvmPackages.llvm pkg-config (python3.withPackages (ps: [ ps.pygobject3 ])) self.packages.${system}.ringboard rust-analyzer rustc rustfmt self.packages.${system}.imageEditor wayland-utils wl-clipboard ];
          GI_TYPELIB_PATH = pkgs.lib.makeSearchPath "lib/girepository-1.0" [ pkgs.gtk3 pkgs.glib pkgs.pango pkgs.gdk-pixbuf pkgs.at-spi2-core pkgs.harfbuzz ];
          LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
          LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";
          RUST_BACKTRACE = "1";
          RUST_LOG = "clip_daemon=debug";
          RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
        };
      });

      formatter = forAllSystems (system: pkgs: pkgs.nixpkgs-fmt);
    };
}
