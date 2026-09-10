# matari-audio-drag-and-drop

[![CI](https://github.com/DerpcatMusic/matari-audio-drag-and-drop/actions/workflows/ci.yml/badge.svg)](https://github.com/DerpcatMusic/matari-audio-drag-and-drop/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV 1.95](https://img.shields.io/badge/MSRV-1.95-orange.svg)](#installation)

Typed, toolkit-neutral drag-and-drop for Rust audio plug-in editors.

A plug-in editor is a child window inside somebody else's process. Drag-and-drop
there is not the desktop-app problem with a different window handle: the editor
does not own the toplevel, the host may be XWayland-bridged, and the initiating
gesture belongs to a toolkit Matari does not control. This crate is the part of
that problem that is the same for every plug-in and every toolkit.

Matari separates product decisions from native protocol work:

- `Controller` owns one editor's sessions and events.
- `ToolkitAdapter` connects the controller to a live GUI callback.
- `StartTicket` is non-cloneable authority to start exactly one drag.
- `NativeRuntimePort` covers OLE and AppKit runtimes.
- `X11Session` runs XDND on the toolkit-owned X11 connection, or a native
  Wayland data-device source for embedded XWayland editors on Hyprland.
- `WaylandRuntimePort` consumes an event-scoped runtime that owns the
  toolkit's one-use press token and existing Wayland queue.
- `SessionReporter` delivers native lifecycle events without polling, timers,
  or process-global buses.

The package validates file-backed payloads before they reach a native runtime.
Session IDs are direction-typed, outcomes and failures are explicit, and each
toolkit adapter reports only the routes available to its live editor.

## Installation

```sh
cargo add matari-audio-drag-and-drop
```

Or pin the repository directly while tracking an unreleased fix:

```toml
[dependencies]
matari-audio-drag-and-drop = { git = "https://github.com/DerpcatMusic/matari-audio-drag-and-drop", tag = "v0.2.0" }
```

Requires Rust 1.95 (edition 2024). Native dependencies are pulled in per target
and nothing extra is needed on Windows or macOS. Building for Linux needs the
Wayland development headers:

```sh
sudo apt-get install libwayland-dev      # Debian/Ubuntu
sudo pacman -S wayland                   # Arch
sudo dnf install wayland-devel           # Fedora
```

## Quick start

Most integrations should reach for [`toolkit::X11Host`] rather than wiring the
facade by hand. It owns the pointer authority, the live outbound session, and
the inbound drop router that an embedded editor needs, behind four calls:

```rust,ignore
use matari_audio_drag_and_drop::toolkit::X11Host;

// once, after the editor window is mapped
let mut matari = X11Host::new();
matari.attach(conn, editor_window)?;

// first thing in the toolkit's X11 event handler
if matari.handle_event(conn, &event)? {
    return; // the event was Matari's, not the toolkit's
}

// from the GUI callback that owns the drag gesture
matari.start_drag(conn, DragOrigin::from_window(window)?, ticket)?;

// before the editor window is destroyed
matari.shutdown(conn);
```

Windows and macOS need no host: pass the [`StartTicket`] straight to
[`NativeRuntime`], whose OLE and AppKit runtimes own their own message pumps.

## What an integration must provide

| Piece | X11/XWayland | Windows | macOS |
| --- | --- | --- | --- |
| Outbound start | `X11Host::start_drag` | `NativeRuntime` | `NativeRuntime` |
| Native event pump | `X11Host::handle_event` on the toolkit's queue | OLE, owned by the runtime | AppKit, owned by the runtime |
| Inbound offers | the toolkit's own XDND target | `IDropTarget` | `NSDraggingDestination` |
| **Inbound routing for embedded editors** | **`X11Host::attach`** | not needed | not needed |
| Pointer authority | tracked by `X11Host` | implicit in the OLE call | the `NSEvent`, via `DragOrigin::with_appkit_event` |

The routing row is the one that gets skipped, because every other row can be
wired correctly and inbound drops will still never arrive. Under Hyprland's
serial-less XWayland bridge the compositor delivers the drop to the *host's*
toplevel window, not to the `XdndAware` plug-in child inside it. `attach`
installs an `XdndProxy` that forwards it on, and returns `Ok(false)` on the
compositors where that would be redundant. If drops work under KWin but not
Hyprland, this call is missing.

## Integration shape

An editor owns a `Controller`, constructs a `FileSet` after its background
export completes, and passes it to `Controller::start_outbound`. Its
`ToolkitAdapter` schedules the owned `StartTicket` from the current GUI gesture.
The native runtime retains the supplied reporter and finishes the session from
real platform callbacks.

Native Wayland starts consume an event-scoped runtime that owns the toolkit's
one-use press token and existing event queue.

X11/XWayland starts consume the initiating pointer event and return an
`X11Session`. The adapter selects the route before consuming the start ticket.
XDND sessions use events forwarded from the owning X11 queue. Matari detects
the compositor bridge explicitly: KWin, Mutter, and COSMIC use canonical XDND;
Hyprland uses its serial-less native Wayland compatibility route only when both
the live X window manager and session identify Hyprland; unknown or unsupported
bridges remain XDND-only. A live xwayland-satellite server overrides stale
desktop labels because it does not bridge DND between X11 and native Wayland.
XDND target discovery follows the X server's live child-under-pointer ancestry
instead of reconstructing target placement from root-window rectangles. The
route never infers completion from elapsed time.

Compositors without an X11-source to native-Wayland bridge cannot make an
embedded XWayland editor drop into native Wayland targets. Those sessions need
a native Wayland editor with a real toolkit press serial; XDND remains
available for X11/XWayland targets.

Linux reporters distinguish successful export, target/compositor rejection,
unsupported routes, and setup failure.

## Maturity

Matari 0.1 is experimental. Windows OLE, macOS AppKit, X11/XWayland XDND, and
native Wayland data-device integrations compile on their native targets, but
compilation is not host qualification. Treat a route as supported only after a
dated run records the operating system, host and format, window presentation,
source and target backends, payload, and exact terminal outcome.

## Realtime safety

Matari is a GUI/background facility. Do not validate files, start a native
drag, access the OS, or drain controller events from an audio callback.

## Development

```sh
cargo fmt --check
cargo check --locked --all-features
cargo test --all-features
cargo clippy --locked --lib --all-features -- -D warnings
cargo publish --dry-run --locked
```

CI runs these on Linux, Windows, and macOS, against stable and the declared
MSRV, plus `cargo-deny` for advisories, licences, and sources. Native
backends only compile on their own target, so a change to one backend needs
the matching job green before review.

See [CONTRIBUTING.md](CONTRIBUTING.md) for protocol invariants and
[SECURITY.md](SECURITY.md) for private vulnerability reporting.

## License

[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE)
