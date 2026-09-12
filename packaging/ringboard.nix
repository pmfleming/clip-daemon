{ pkgs }:
pkgs.ringboard-wayland.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./ringboard-policy.patch ];
  postPatch = (old.postPatch or "") + ''
    cp ${./ringboard-policy/policy_allocator.rs} server/src/policy_allocator.rs
  '';
  RUSTC_BOOTSTRAP = "clipboard_history_core,clipboard_history_client_sdk";
})
