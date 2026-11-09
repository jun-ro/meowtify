//! iOS entry point. Phase 1 spike: a one-screen Slint app.
//!
//! It answers three questions before any real porting starts:
//! 1. Does the iOS toolchain build and package without Xcode on this machine?
//! 2. Does Slint run on iOS, and with which renderer (Skia on Metal, or software)?
//! 3. Does a sideloaded build install and start on the device?

#[cfg(target_os = "ios")]
mod app {
    slint::slint! {
        export component Spike inherits Window {
            in property <string> renderer;
            in-out property <int> taps;
            background: #14111f;
            VerticalLayout {
                alignment: center;
                spacing: 16px;
                padding: 24px;
                Text {
                    text: "Meowtify";
                    color: white;
                    font-size: 32px;
                    horizontal-alignment: center;
                }
                Text {
                    text: "iOS spike. Renderer: " + root.renderer;
                    color: #b9b4d6;
                    font-size: 16px;
                    horizontal-alignment: center;
                }
                Rectangle {
                    height: 64px;
                    background: #7c5cff;
                    border-radius: 12px;
                    TouchArea {
                        clicked => { root.taps += 1; }
                    }
                    Text {
                        text: "Taps: " + root.taps;
                        color: white;
                        font-size: 20px;
                    }
                }
            }
        }
    }

    pub fn run() -> Result<(), slint::PlatformError> {
        let renderer = if cfg!(feature = "software") { "software" } else { "skia" };
        slint::BackendSelector::new()
            .backend_name("winit".to_string())
            .renderer_name(renderer.to_string())
            .select()?;
        let ui = Spike::new()?;
        ui.set_renderer(renderer.into());
        ui.run()
    }
}

#[cfg(target_os = "ios")]
fn main() {
    if let Err(err) = app::run() {
        eprintln!("meowtify-ios: {err}");
    }
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("meowtify-ios only runs on iOS. Build it with --target aarch64-apple-ios.");
}
