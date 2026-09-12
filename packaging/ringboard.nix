{ pkgs }:
pkgs.ringboard-wayland.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./ringboard-policy.patch ];
  cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
    inherit (old) src;
    patches = [ ./ringboard-policy.patch ];
    hash = "sha256-kbVROjJcexvQJKEnMt1EMgF6ZC5MjB3IxPlam0Gcg30=";
  };
  postPatch = (old.postPatch or "") + ''
    cp ${./ringboard-policy/policy_allocator.rs} server/src/policy_allocator.rs
    cp ${./ringboard-policy/limit.rs} server/src/capture_limit.rs
    cp ${./ringboard-policy/limit.rs} wayland/src/capture_limit.rs
  '';
  meta = old.meta // { license = [ pkgs.lib.licenses.agpl3Only pkgs.lib.licenses.asl20 ]; };
  RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
})
