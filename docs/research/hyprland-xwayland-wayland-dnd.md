# Hyprland XWayland-to-Wayland drag-and-drop

## Question

Why can a plug-in editor embedded as an X11/XWayland plug-in window export a file to a native Wayland target on one gesture, then request the payload without completing the next gesture on Hyprland 0.56.1?

## Verdict

The strongest supported cause is an ordering race between Matari reporting that a native Wayland drag has started and Hyprland actually processing `wl_data_device.start_drag`.

Matari's compatibility bridge previously called `flush()` and immediately returned success to the X11 editor. `flush()` only writes queued requests to the Wayland socket. Hyprland does not install its global mouse-release listener until it processes `start_drag`. A release in that interval is therefore invisible to Hyprland's drag lifecycle. The target may already request the file payload, but Hyprland never performs or finishes the drop. That is the exact terminal-event pattern in the reported logs.

The minimal change is to call `Connection::roundtrip()` after `start_drag` and before reporting success. The Wayland display sync used by `roundtrip()` is processed after all preceding requests, so a successful return proves that Hyprland has processed the drag start. This is protocol and source-code evidence for the race and the synchronization point; it is not runtime proof that every observed failure is fixed. Repeated real plug-in gestures are still required.

## Actual stack

The failing path is not the plug-in's standalone/native Wayland path:

1. A Nice Plug editor is embedded by the host as an X11 window under XWayland.
2. Baseview captures the X11 pointer gesture and gives Matari X11 pointer authority.
3. Matari opens a separate native Wayland client and asks Hyprland to start a drag with a roleless origin surface and serial zero.
4. Hyprland-specific behavior accepts that otherwise non-standard request and exposes the file to a native Wayland target.

The standalone path works because its Winit/Baseview Wayland window owns the real `wl_surface`, seat, and press serial. The embedded compatibility bridge cannot obtain a valid Wayland serial for an X11 gesture.

The installed environment inspected for this report was Hyprland 0.56.1 at commit `5c9377c15f85c50648f35ca5a213754f95b93ca0`, XWayland 24.1.13, Wayland 1.25, wayland-protocols 1.49, xdg-desktop-portal 1.22.1, and xdg-desktop-portal-hyprland 1.4.1.

## Race proof

### Client side

`wayland-client` documents `Connection::flush()` as sending pending requests, while `Connection::roundtrip()` sends a display sync and waits until the server has processed every preceding request. The former establishes transport progress; the latter establishes server processing order. [wayland-client 0.31.15 source](https://docs.rs/wayland-client/0.31.15/src/wayland_client/conn.rs.html#120-175)

Before this change, Matari performed these operations in order:

1. create and configure the data source;
2. submit `wl_data_device.start_drag`;
3. store the active drag state;
4. flush the connection;
5. tell the X11 caller that the drag had started.

That sequence allowed the physical button release to overtake compositor-side drag initialization after step 5.

### Hyprland side

Hyprland 0.56.1 receives `start_drag`, then enters `initiateDrag`. It installs its global mouse-button release listener inside that processing path. The release listener is what later calls `dropDrag`; successful target completion is what sends the data source's `dnd_drop_performed` and `dnd_finished` events. [start-drag handler](https://github.com/hyprwm/Hyprland/blob/v0.56.1/src/protocols/core/DataDevice.cpp#L264-L279), [listener installation](https://github.com/hyprwm/Hyprland/blob/v0.56.1/src/protocols/core/DataDevice.cpp#L557-L664), [drop and completion](https://github.com/hyprwm/Hyprland/blob/v0.56.1/src/protocols/core/DataDevice.cpp#L737-L799)

Hyprland ignores the supplied serial in this handler. That is the compositor-specific behavior that lets Matari's serial-zero bridge start at all; it does not remove the listener-registration race.

A target requesting data is not proof that a drop completed. The Wayland data-device protocol permits data transfer before the physical drop and defines separate `dnd_drop_performed` and `dnd_finished` lifecycle events. [Wayland 1.24 core protocol](https://gitlab.freedesktop.org/wayland/wayland/-/blob/1.24.0/protocol/wayland.xml)

## Why the roundtrip is safe in this worker

The Matari connection and its calloop event loop live on one dedicated worker thread. The roundtrip runs from a calloop command-channel callback on that same worker.

`calloop-wayland-source` uses a prepared read guard while the event loop sleeps, but explicitly drops that guard in `before_handle_events` before any source callback is dispatched so callback code may use the Wayland socket. Calloop invokes this lifecycle phase for all registered sources before dispatching source callbacks. Consequently, the command callback cannot enter `Connection::roundtrip()` while `WaylandSource` still owns the prepared read guard. [calloop-wayland-source 0.4.1](https://github.com/Smithay/calloop-wayland-source/blob/v0.4.1/src/lib.rs#L183-L218)

`Connection::roundtrip()` waits on the backend display-sync queue rather than recursively dispatching Matari's application event queue. It therefore does not re-enter `BridgeState` drag callbacks. The connection remains owned by the one worker thread during the operation.

There is one operational caveat: if the compositor wedges, `Connection::roundtrip()` can block the worker. The caller's existing one-second response timeout does not cancel that underlying roundtrip. This is preferable to falsely reporting a usable drag, but it should remain visible in diagnostics.

## Why the observed alternation is plausible

When the release is missed, the previous drag remains incomplete. The next gesture has to supersede or abort that stale drag before Hyprland can establish a new one, which can make success and failure look alternating. The reported sequence—target requests data, no exported/finished terminal event, then a later drag works—is consistent with this lifecycle, but it is not sufficient to prove that every alternating gesture has one cause.

The bridge also renders and allocates its Wayland drag icon synchronously before `start_drag`, after the X11 gesture has already crossed its drag threshold. This widens the interval between the user's physical gesture and Hyprland registering the release listener. If the processing barrier does not eliminate the runtime failures, pre-rendering or preallocating the icon before the second gesture is the next narrow optimization. A sleep is not a substitute for either change because it provides no compositor-processing guarantee.

## Routes that cannot replace this bridge

### Canonical XDND

Hyprland 0.56.1's XWayland bridge implements the native-Wayland-source to XWayland-target direction. Its XWM ignores XFixes notifications for the X11 DND selection instead of constructing a native Wayland data source from an X11 drag. [Hyprland XWM selection handling](https://github.com/hyprwm/Hyprland/blob/v0.56.1/src/xwayland/XWM.cpp#L795-L807), [Hyprland PR 8708](https://github.com/hyprwm/Hyprland/pull/8708)

Therefore canonical XDND is appropriate for XWayland destinations but cannot, on this Hyprland version, initiate the reverse XWayland-source to native-Wayland-target route an embedded XWayland plug-in needs.

### XDG Desktop Portal FileTransfer

The FileTransfer portal exports and retrieves files through a transfer key. It has no API to begin a pointer drag, supply a Wayland seat serial, or drive data-device enter/drop/finish events. It can broker payload access after a drag exists, but cannot replace `wl_data_device.start_drag`. [FileTransfer portal API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileTransfer.html)

### Persistent connection reuse

Keeping the native Wayland connection alive is correct for source lifetime and avoids per-gesture setup churn. It does not guarantee that Hyprland processed a specific `start_drag` request before the X11 caller resumed, so it does not close this race by itself.

### Retrying or sleeping

A retry after failure only masks a stale lifecycle and risks duplicate drags. A fixed delay may make the race less frequent but cannot prove that the compositor processed the request. A same-connection display sync is the smallest precise barrier.

## Gold-standard route

The standards-compliant solution is a native Wayland floating CLAP editor. CLAP explicitly states that Wayland embedding is unsupported and Wayland editors should use floating windows. A native floating editor owns the actual Wayland surface and the implicit-grab serial, allowing the existing Winit/Baseview drag path to use the protocol as designed. [CLAP GUI extension](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/ext/gui.h#L62-L68)

That route changes host/editor window behavior and does not solve VST3 hosts that expose only X11 embedding, so it is not a drop-in shared-crate patch. The Hyprland serial-zero bridge remains a compatibility workaround for embedded X11 plug-in windows, not a portable Wayland implementation.

An independent implementation found the same general failure class: kitty moved asynchronous Wayland drag start behind active implicit-grab validation because starting after mouse release could be ignored without terminal source events. [kitty PR 10136](https://github.com/kovidgoyal/kitty/pull/10136)

## Validation boundary

The source inspection proves:

- `flush()` did not wait for Hyprland to process `start_drag`;
- Hyprland registers release handling only while processing that request;
- `roundtrip()` is the correct same-connection processing barrier;
- the calloop prepared-read lifecycle does not conflict with invoking it from the command callback;
- XDND and FileTransfer do not provide the required reverse drag-start route.

It does not prove that the user's alternating failure is gone. That requires repeated drags from the installed plug-in to both native Wayland and XWayland targets, with logs confirming each drag reaches a terminal exported, cancelled, or failed state rather than stopping after `Target requested file data`.
