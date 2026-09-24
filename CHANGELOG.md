# Changelog

## 0.1.6 - Unreleased

- Preserve the X11 source gesture through Hyprland's native Wayland bridge so a completed XWayland drop always releases the editor and later drags keep working.
- Keep the Windows source thumbnail visible and anchored across DAW drop targets that do not implement the Shell target helper.
- Report the native result code and drop effect on failed Windows transfers so hosts can distinguish a refused drop from a transport failure.
- Draw the Windows drag thumbnail in an own layered window at the plugin window's DPI: fully opaque, same size as the in-app chip, and anchored at the chip's cursor offset instead of the image-list ghost that was translucent, fixed at 224x90 physical pixels, and jumped at handoff.
- Ignore inbound offers that are this controller's own live outbound drag reflected back by AppKit or an XWayland window manager, so an editor can no longer reject and cancel its own export.
- Stand down the XWayland drop router while this crate holds a live outbound drag, dropping bridged `XdndEnter`, `XdndPosition` and `XdndDrop` messages that name our own source window without stranding a phantom offer.

## 0.1.5 - Unreleased

- Route compositor-bridged file drops through XWayland host windows to embedded plug-in editors without adding another native event loop.
- Retire a compositor-stuck Wayland drag when the next pointer gesture begins so one failed destination cannot block later drags.

## 0.1.4 - Unreleased

- Wait for Hyprland to process native Wayland drag setup before returning control to an XWayland editor, preventing pointer release from overtaking compositor setup.

## 0.1.3 - Unreleased

- Keep Hyprland's serial-less Wayland data-device runtime alive across XWayland plug-in drags instead of rebuilding its connection for every gesture.

## 0.1.2 - Unreleased

- Route Hyprland XWayland editors through the compositor's canonical XDND bridge, preventing serial-less private Wayland sessions from leaving alternating drag gestures unfinished.
- Render spectral drag previews with smooth high-detail sampling and a clearer multicolor energy palette.

## 0.1.1 - Unreleased

- Render native drag previews as rounded, high-contrast dark cards so toolkit and desktop handoff previews remain visually consistent.
- Expose explicit KWin/Mutter/COSMIC canonical-XDND, Hyprland serial-less-Wayland, and unavailable cross-display capabilities instead of presenting one compositor workaround as universal.
- Detect xwayland-satellite from the live X server and keep those editors on XDND even when launchers expose stale desktop labels.
- Keep XDND host targets reachable when independently positioned XWayland editor and DAW windows overlap in X11 coordinates.
- Require both a live Hyprland X window manager and session before selecting its serial-less native Wayland compatibility route.
- Route embedded XWayland editors on Hyprland through a native Wayland data-device source, including source previews and event-driven lifetime through late payload requests.
- Keep Hyprland native-to-XWayland transfers replaceable after full payload delivery even when the compositor omits drop and finish callbacks, while retaining the source for late requests.
- Keep event-driven X11 pointer authority and accepted-target evidence through button release so crossing onto a native Wayland surface cannot strand the drag.
- Restore waveform, spectrogram, and MIDI source previews for event-driven X11/XWayland drags.
- Add a reusable native-Wayland lifecycle reporter with distinct progress, replaceability, and terminal events.
- Coalesce repeated native progress notifications into one data-request and one drop-performed event per drag.
- Free Windows transfer allocations when native memory locking fails.
- Document native unsafe invariants and the public X11 event lifecycle.
- Enforce the declared Rust 1.95 MSRV, dependency policy, strict native unsafe
  lints, complete public documentation, and registry publication dry runs.
- Mark every native backend experimental until dated host evidence qualifies it.

## 0.1.0 - 2026-07-30

- Introduced Matari's editor-owned, typed drag-and-drop API with event-driven native lifecycle reporting.
- Fixed proxied XDND delivery and wait for the target's exact accept or reject event before sending a drop.
- X11/XWayland drag sessions now run on the editor's native event queue with no side connection or pointer polling.
- Native drag completion now wakes the editor immediately and preserves exact start-to-finish event order without polling or delay timers.
- Improved macOS drag-session cleanup after completed or cancelled drops.
