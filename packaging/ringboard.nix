{ pkgs }:
pkgs.ringboard-wayland.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./ringboard-policy.patch ];
  postPatch = (old.postPatch or "") + ''
    cp ${./ringboard-policy/policy_allocator.rs} server/src/policy_allocator.rs
    cp ${./ringboard-policy/limit.rs} server/src/capture_limit.rs
    cp ${./ringboard-policy/limit.rs} wayland/src/capture_limit.rs
  '';
  RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
})
