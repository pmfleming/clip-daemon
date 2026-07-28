{
  description = "Clipboard policy and clip-api daemon for Shelllist";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (system: pkgs:
        let
          clipDaemon = pkgs.rustPlatform.buildRustPackage {
            pname = "clip-daemon";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
            buildInputs = [ pkgs.dbus ];
            strictDeps = true;
            # Ringboard 0.16.2 still declares core_io_borrowed_buf as nightly-only.
            RUSTC_BOOTSTRAP = "1";
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
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.coreutils pkgs.grim pkgs.hyprland pkgs.libnotify pkgs.satty pkgs.systemd pkgs.xdg-utils ]}
            '';
            meta = {
              description = "Wayland clipboard policy and clip-api daemon for Shelllist";
              mainProgram = "clip-daemon";
              platforms = pkgs.lib.platforms.linux;
            };
          };
        in {
          default = clipDaemon;
          ringboardQualification = pkgs.writeShellApplication {
            name = "clip-daemon-ringboard-qualification";
            runtimeInputs = [ pkgs.jq pkgs.ringboard-wayland pkgs.wayland-utils ];
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
          packages = with pkgs; [ cargo cargo-llvm-cov clippy dbus grim jq just llvmPackages.llvm pkg-config ringboard-wayland rust-analyzer rustc rustfmt wayland-utils ];
          LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
          LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";
          RUST_BACKTRACE = "1";
          RUST_LOG = "clip_daemon=debug";
          RUSTC_BOOTSTRAP = "1";
        };
      });

      formatter = forAllSystems (system: pkgs: pkgs.nixpkgs-fmt);
    };
}
