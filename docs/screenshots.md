# Explicit screenshots

`clip-daemon screenshot [--screen] [--annotate]` requests an explicit, daemon-owned
capture. By default Slurp selects a region; `--screen` captures all outputs. These
are deliberate clipboard writes, not background history capture: pause/private
mode continues to control whether Ringboard records the result.

The additive `clipboard.capture.interactive` method accepts `mode: region|screen`
and `annotate: bool` (default false), returning a tracked operation. Only one
interactive screenshot can run at a time. Operation events and cancellation use
the existing clipboard operation protocol. The launching CLI may exit while the
job continues. Selection cancellation never falls back to capturing all outputs.

The service launches fixed Slurp/Grim commands without a shell, validates selected
geometry and bounded PNG data, and optionally runs the configured image editor.
Satty must return an output file; it does not own the clipboard. No clipboard
history entry needs to exist before annotation. Temporary files are private and
removed on completion/failure/cancellation. Selection, capture and annotation
have bounded lifetimes. Cancellation cannot interrupt a publication already in
its commit phase. Clipboard publication remains owned by clip-daemon.

Deploy the daemon package and keybindings together. No additional service is
needed. Do not retain the old screenshot-annotate helper or grim/wl-copy pipelines.
