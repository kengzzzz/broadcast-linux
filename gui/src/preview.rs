use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use broadcast_linux::webcam::{self, Capture, Format};
use eframe::egui;

/// Reads the virtual camera like any app would, so the service runs the effects while it is open.
pub struct Preview {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
    texture: Option<egui::TextureHandle>,
}

#[derive(Default)]
struct Shared {
    frame: Option<egui::ColorImage>,
    error: Option<String>,
}

impl Preview {
    pub fn start(device: String, ctx: egui::Context) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let thread = thread::spawn({
            let (stop, shared) = (Arc::clone(&stop), Arc::clone(&shared));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    // The service restarting ends the capture; open it again.
                    let result = capture(&device, &stop, &shared, &ctx);
                    shared.lock().unwrap().error = result.err().map(|e| format!("{e:#}"));
                    ctx.request_repaint();
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
            shared,
            texture: None,
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared.lock().unwrap().error.clone()
    }

    pub fn texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if let Some(frame) = self.shared.lock().unwrap().frame.take() {
            match &mut self.texture {
                Some(texture) => texture.set(frame, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("preview", frame, egui::TextureOptions::LINEAR));
                }
            }
        }
        self.texture.as_ref()
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn capture(
    device: &str,
    stop: &AtomicBool,
    shared: &Mutex<Shared>,
    ctx: &egui::Context,
) -> Result<()> {
    let (width, height) = webcam::capture_size(device)?;
    let mut capture = Capture::open(device, Format::Yuyv, width, height, 30)?;
    while !stop.load(Ordering::Relaxed) {
        let image = capture.newest(Duration::from_millis(100), |frame| {
            half_size_rgba(frame, width as usize, height as usize)
        })?;
        if let Some(image) = image {
            let mut shared = shared.lock().unwrap();
            shared.frame = Some(image);
            shared.error = None;
            ctx.request_repaint();
        }
    }
    Ok(())
}

/// One pixel per YUYV pair on every other row, in BT.601 limited range.
fn half_size_rgba(yuyv: &[u8], width: usize, height: usize) -> egui::ColorImage {
    let (out_w, out_h) = (width / 2, height / 2);
    let mut rgba = Vec::with_capacity(out_w * out_h * 4);
    for row in 0..out_h {
        let start = row * 2 * width * 2;
        let Some(line) = yuyv.get(start..start + width * 2) else {
            rgba.resize(out_w * out_h * 4, 0);
            break;
        };
        for [y0, cb, y1, cr] in line.as_chunks::<4>().0 {
            let luma = 1.164 * (f32::midpoint(f32::from(*y0), f32::from(*y1)) - 16.0);
            let (blue, red) = (f32::from(*cb) - 128.0, f32::from(*cr) - 128.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let clamp = |x: f32| x.clamp(0.0, 255.0).round() as u8;
            rgba.extend_from_slice(&[
                clamp(luma + 1.596 * red),
                clamp(luma - 0.392 * blue - 0.813 * red),
                clamp(luma + 2.017 * blue),
                255,
            ]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([out_w, out_h], &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_yuyv() {
        let black = [16u8, 128, 16, 128].repeat(8);
        let image = half_size_rgba(&black, 4, 4);
        assert_eq!(image.size, [2, 2]);
        assert!(image.pixels.iter().all(|p| p.r() == 0 && p.a() == 255));
        let white = [235u8, 128, 235, 128].repeat(8);
        let image = half_size_rgba(&white, 4, 4);
        assert!(image.pixels.iter().all(|p| p.r() == 255 && p.g() == 255));
    }
}
