//! Print what Matari detects about this desktop session.
//!
//! Run this on the machine where a drag misbehaves and paste the output into
//! a bug report:
//!
//! ```sh
//! cargo run --example bridge-report
//! ```

fn main() {
    #[cfg(all(target_family = "unix", not(target_os = "macos")))]
    {
        let report = matari_audio_drag_and_drop::x11_bridge_report();
        println!("bridge:   {:?}", report.bridge);
        println!("evidence: {}", report.evidence.summary());
        println!("protocol: {:?}", report.protocol());
        println!(
            "inbound drop router required: {}",
            report.bridge
                == matari_audio_drag_and_drop::X11WaylandBridge::HyprlandSeriallessCompat
        );
    }

    #[cfg(not(all(target_family = "unix", not(target_os = "macos"))))]
    println!("Bridge detection is Linux-only; this target uses its native runtime.");
}
