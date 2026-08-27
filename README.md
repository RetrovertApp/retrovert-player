# Retrovert Player

Shared headless playback engine for Retrovert hosts.

The Cargo workspace contains three crates:

- `retrovert-player`: the headless playback engine
- `retrovert-plugin-catalog`: keeps the engine's decoder set current against an update channel
- `retrovert-player-desktop`: the desktop player binary, following the `dev` channel

`retrovert-player` owns headless decode coordination. Renderers are not part of this
workspace: the player UI draws with flowi and lives with the host that embeds it.
`retrovert-plugin-catalog` owns its own `retrovert-updater` instance and drives
check → apply → activate → retain from one `tick()` on the audio worker's loop;
generations activate all-or-nothing at the mount/stop boundary with a
reload-previous fallback. `retrovert-player-desktop` wires the engine and the
catalog to a cpal output and a stdin command loop.

## Building

All dependencies resolve from a plain clone; no sibling checkouts are needed. From the
repository root, run:

```sh
./scripts/check.sh
```

Original code is MIT licensed.
