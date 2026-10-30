{
  description = "Turso-backed Wayland clipboard policy and clip-api daemon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.daemonFramework = {
    url = "git+file:../daemon-framework";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, daemonFramework }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in {
      packages = forAllSystems (system: pkgs:
        let
          satty = pkgs.satty.overrideAttrs (old: {
            patches = (old.patches or [ ]) ++ [ ./packaging/satty-toolbar-layout.patch ];
          });
          clipDaemon = daemonFramework.lib.buildRustPackage pkgs {
            pname = "clip-daemon";
            version = "0.1.0";
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml ./Cargo.lock ./flake.nix ./src ./tests ./test_support ./examples
                ./packaging/systemd ./packaging/dbus ./vendor/tantivy-sstable
              ];
            };
            postUnpack = ''
              cp -R --no-preserve=mode ${daemonFramework.lib.daemonSource pkgs} "$(dirname "$sourceRoot")/daemon-framework"
            '';
            # Crane's dependency skeleton replaces path crates with empty libs.
            # This patched third-party crate is consumed by registry dependencies,
            # so its real public API is needed in the dependency build as well.
            postPatch = ''
              rm -rf vendor/tantivy-sstable
              cp -R --no-preserve=mode ${./vendor/tantivy-sstable} vendor/tantivy-sstable
            '';
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config pkgs.clang ];
            buildInputs = [ pkgs.dbus ];
            nativeCheckInputs = [ pkgs.dbus ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            strictDeps = true;
            # Include private large-frame tests; the native Quickshell probe is
            # excluded by its non-default feature and runs separately.
            checkFlags = [ "--include-ignored" "--test-threads=1" ];
            postInstall = ''
              install -Dm644 ${./integrations/yazi/yank-to-clip-daemon.yazi/main.lua} \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua
              substituteInPlace $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua \
                --replace-fail @out@ $out
              ${pkgs.lua}/bin/lua ${./integrations/yazi/test.lua} \
                $out/share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua $out/bin/clip-daemon
              install -Dm644 ${./packaging/systemd/clip-daemon.service} \
                $out/share/systemd/user/clip-daemon.service
              install -Dm644 ${./packaging/dbus/org.laufan.ClipDaemon.service} \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service
              substituteInPlace $out/share/systemd/user/clip-daemon.service \
                $out/share/dbus-1/services/org.laufan.ClipDaemon.service \
                --replace-fail @out@ $out
            '';
            postFixup = ''
              wrapProgram $out/bin/clip-daemon --prefix PATH : ${pkgs.lib.makeBinPath [
                pkgs.coreutils pkgs.grim pkgs.slurp pkgs.hyprland pkgs.libnotify
                satty pkgs.xdg-utils
              ]}
            '';
            meta = {
              description = "Single-owner Wayland clipboard with embedded Turso history";
              mainProgram = "clip-daemon";
              platforms = systems;
              license = pkgs.lib.licenses.mit;
            };
          };
        in {
          default = clipDaemon;
          imageEditor = satty;
        });
      apps = forAllSystems (system: _: {
        default = { type = "app"; program = "${self.packages.${system}.default}/bin/clip-daemon"; };
      });
      checks = forAllSystems (system: _: { default = self.packages.${system}.default; });
      devShells = forAllSystems (system: pkgs: {
        default = pkgs.mkShell {
          packages = (daemonFramework.lib.toolchain pkgs).packages ++ (with pkgs; [
            cargo-audit cargo-machete cargo-llvm-cov clang dbus ghostty git grim
            hyprland just llvmPackages.llvm pkg-config quickshell rust-analyzer
            self.packages.${system}.imageEditor wayland-utils wl-clipboard
          ]);
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
          LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";
          RUST_BACKTRACE = "1";
          RUST_LOG = "clip_daemon=debug";
        };
      });
      formatter = forAllSystems (_: pkgs: pkgs.nixpkgs-fmt);
    };
}
