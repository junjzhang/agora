# nwm — niri workspace manager

Agent-era workspace manager for the [niri](https://github.com/YaLTeR/niri) compositor.

> 🚧 v0.1 / MVP scaffolding — design lives in [`VISION.html`](VISION.html).

## Quick start

```sh
pixi run build           # cargo build
pixi run nwm -- --help   # CLI
pixi run nwmd            # daemon (stub)
pixi run check           # fmt + clippy + test
```

## Layout

- `src/lib.rs` + `src/model.rs` — shared model (Project / Root / Launcher)
- `src/bin/nwmd.rs` — daemon
- `src/bin/nwm.rs` — CLI
