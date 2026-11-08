//! The animated logo. Animated GIFs rotate: music clips while a song plays, idle
//! clips otherwise. Each frame goes to two places: the logo next to the title in
//! the sidebar, and the window icon, which is the taskbar icon.
//!
//! Slint does not animate GIFs, so each clip is decoded to frames and a timer
//! shows them. Only the clip on screen is decoded (about 2 MB at most); the files
//! stay compressed inside the program (about 160 KB in all).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use std::time::Instant;

use slint::winit_030::{winit, WinitWindowAccessor};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode};

use crate::MainWindow;

/// Side of the square the clips are drawn into, in pixels. The art is 24 art
/// pixels wide, so 72 gives exactly 3 pixels per art pixel. The logo shows it at
/// 36 pixels, so a 2x screen shows it one to one.
const SIDE: usize = 72;
/// The window icon changes at most this often. Each change makes Windows build a
/// new icon and redraw the taskbar button.
const ICON_EVERY: Duration = Duration::from_millis(120);
/// A clip repeats until it has been on screen about this long.
const SHOW_MS: u32 = 6000;
/// The play state must hold this long before the clips change, so the gap
/// between two songs does not flash the idle clips.
const SETTLE: Duration = Duration::from_millis(900);
/// Frames shorter than this are slowed down, so a broken delay cannot spin the UI.
const MIN_FRAME_MS: u32 = 40;

static MUSIC: [&[u8]; 3] = [
    include_bytes!("../../../assets/icon/gif/meowtify-dance.gif"),
    include_bytes!("../../../assets/icon/gif/meowtify-vibe.gif"),
    include_bytes!("../../../assets/icon/gif/meowtify-love.gif"),
];
static IDLE: [&[u8]; 4] = [
    include_bytes!("../../../assets/icon/gif/meowtify-idle.gif"),
    include_bytes!("../../../assets/icon/gif/meowtify-meow.gif"),
    include_bytes!("../../../assets/icon/gif/meowtify-peek.gif"),
    include_bytes!("../../../assets/icon/gif/meowtify-sleep.gif"),
];

struct Clip {
    /// Each frame with how long it shows, in milliseconds.
    frames: Vec<(SharedPixelBuffer<Rgba8Pixel>, u32)>,
    loops: u32,
}

/// Decodes a GIF to square frames of `SIDE` pixels. A clip that is not square is
/// centred across and sits on the bottom edge, so a peeking cat peeks from below.
fn decode(bytes: &[u8]) -> Option<Clip> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut dec = opts.read_info(bytes).ok()?;
    let (w, h) = (dec.width() as usize, dec.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let mut canvas = vec![0u8; w * h * 4];
    let mut frames = Vec::new();
    while let Some(f) = dec.read_next_frame().ok()? {
        let saved = (f.dispose == gif::DisposalMethod::Previous).then(|| canvas.clone());
        let (fl, ft, fw, fh) = (f.left as usize, f.top as usize, f.width as usize, f.height as usize);
        for y in 0..fh {
            for x in 0..fw {
                let (cx, cy) = (fl + x, ft + y);
                let s = (y * fw + x) * 4;
                if cx < w && cy < h && f.buffer[s + 3] != 0 {
                    let d = (cy * w + cx) * 4;
                    canvas[d..d + 4].copy_from_slice(&f.buffer[s..s + 4]);
                }
            }
        }
        frames.push((scale(&canvas, w, h), (u32::from(f.delay) * 10).max(MIN_FRAME_MS)));
        match f.dispose {
            gif::DisposalMethod::Background => {
                for y in 0..fh {
                    for x in 0..fw {
                        let (cx, cy) = (fl + x, ft + y);
                        if cx < w && cy < h {
                            let d = (cy * w + cx) * 4;
                            canvas[d..d + 4].fill(0);
                        }
                    }
                }
            }
            gif::DisposalMethod::Previous => {
                if let Some(c) = saved {
                    canvas = c;
                }
            }
            _ => {}
        }
    }
    if frames.is_empty() {
        return None;
    }
    let total: u32 = frames.iter().map(|(_, ms)| ms).sum();
    Some(Clip { frames, loops: SHOW_MS.div_ceil(total.max(1)).max(1) })
}

/// Nearest-neighbour scale into the `SIDE` square. Keeps hard pixel edges.
fn scale(canvas: &[u8], w: usize, h: usize) -> SharedPixelBuffer<Rgba8Pixel> {
    let side = w.max(h);
    let (dw, dh) = (w * SIDE / side, h * SIDE / side);
    let (ox, oy) = ((SIDE - dw) / 2, SIDE - dh);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(SIDE as u32, SIDE as u32);
    let px = buf.make_mut_bytes();
    px.fill(0);
    for dy in oy..SIDE {
        let sy = (((dy - oy) * 2 + 1) * side / (2 * SIDE)).min(h - 1);
        for dx in ox..ox + dw {
            let sx = (((dx - ox) * 2 + 1) * side / (2 * SIDE)).min(w - 1);
            let (s, d) = ((sy * w + sx) * 4, (dy * SIDE + dx) * 4);
            px[d..d + 4].copy_from_slice(&canvas[s..s + 4]);
        }
    }
    buf
}

pub(crate) struct Mascot {
    win: slint::Weak<MainWindow>,
    frame_timer: Timer,
    switch_timer: Timer,
    clip: RefCell<Option<Clip>>,
    frame: Cell<usize>,
    loops_left: Cell<u32>,
    /// Which clips are in use: music (`true`) or idle.
    music: Cell<bool>,
    /// The latest play state. It becomes `music` once it has held for `SETTLE`.
    wanted: Cell<bool>,
    last_index: Cell<usize>,
    rng: Cell<u64>,
    icon_at: Cell<Option<Instant>>,
}

impl Mascot {
    pub(crate) fn new(win: &MainWindow) -> Rc<Mascot> {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Rc::new(Mascot {
            win: win.as_weak(),
            frame_timer: Timer::default(),
            switch_timer: Timer::default(),
            clip: RefCell::new(None),
            frame: Cell::new(0),
            loops_left: Cell::new(0),
            music: Cell::new(false),
            wanted: Cell::new(false),
            last_index: Cell::new(usize::MAX),
            rng: Cell::new(seed | 1),
            icon_at: Cell::new(None),
        })
    }

    pub(crate) fn start(self: &Rc<Self>) {
        self.pick();
    }

    /// Music is playing or not. The clips change after the state has held a moment.
    pub(crate) fn set_playing(self: &Rc<Self>, on: bool) {
        if self.wanted.get() == on {
            return;
        }
        self.wanted.set(on);
        self.switch_timer.stop();
        if on != self.music.get() {
            let weak = Rc::downgrade(self);
            self.switch_timer.start(TimerMode::SingleShot, SETTLE, move || {
                if let Some(m) = weak.upgrade() {
                    m.music.set(m.wanted.get());
                    m.last_index.set(usize::MAX);
                    m.pick();
                }
            });
        }
    }

    fn random(&self, n: usize) -> usize {
        let mut s = self.rng.get();
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        self.rng.set(s);
        (s % n as u64) as usize
    }

    /// Chooses another clip from the current set and starts it.
    fn pick(self: &Rc<Self>) {
        let pool: &[&[u8]] = if self.music.get() { &MUSIC } else { &IDLE };
        let mut i = self.random(pool.len());
        if pool.len() > 1 && i == self.last_index.get() {
            i = (i + 1) % pool.len();
        }
        let Some(clip) = decode(pool[i]) else { return };
        self.last_index.set(i);
        self.loops_left.set(clip.loops);
        self.frame.set(0);
        *self.clip.borrow_mut() = Some(clip); // the old clip's frames are freed here
        self.show();
        self.schedule();
    }

    fn show(&self) {
        let buf = {
            let c = self.clip.borrow();
            let Some(c) = c.as_ref() else { return };
            c.frames[self.frame.get()].0.clone()
        };
        let Some(w) = self.win.upgrade() else { return };
        w.set_mascot(Image::from_rgba8(buf.clone()));
        let due = self.icon_at.get().is_none_or(|t| t.elapsed() >= ICON_EVERY);
        if due {
            let icon = winit::window::Icon::from_rgba(buf.as_bytes().to_vec(), SIDE as u32, SIDE as u32).ok();
            // None until the OS window exists; the next frame tries again.
            let r = w.window().with_winit_window(|win| {
                // The small icon is the title bar's; the taskbar button uses the big one.
                win.set_window_icon(icon.clone());
                #[cfg(windows)]
                winit::platform::windows::WindowExtWindows::set_taskbar_icon(win, icon);
            });
            if r.is_some() {
                self.icon_at.set(Some(Instant::now()));
            }
        }
    }

    fn schedule(self: &Rc<Self>) {
        let ms = {
            let c = self.clip.borrow();
            let Some(c) = c.as_ref() else { return };
            c.frames[self.frame.get()].1
        };
        let weak = Rc::downgrade(self);
        self.frame_timer.start(TimerMode::SingleShot, Duration::from_millis(u64::from(ms)), move || {
            if let Some(m) = weak.upgrade() {
                m.tick();
            }
        });
    }

    fn tick(self: &Rc<Self>) {
        let len = self.clip.borrow().as_ref().map_or(0, |c| c.frames.len());
        if len == 0 {
            return;
        }
        let next = self.frame.get() + 1;
        if next >= len {
            let left = self.loops_left.get().saturating_sub(1);
            self.loops_left.set(left);
            if left == 0 {
                self.pick();
                return;
            }
            self.frame.set(0);
        } else {
            self.frame.set(next);
        }
        self.show();
        self.schedule();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clip_decodes_to_square_frames() {
        for (name, bytes) in MUSIC.iter().chain(IDLE.iter()).enumerate() {
            let clip = decode(bytes).unwrap_or_else(|| panic!("clip {name} did not decode"));
            assert!(clip.frames.len() > 1, "clip {name} has one frame");
            assert!(clip.loops >= 1);
            for (buf, ms) in &clip.frames {
                assert_eq!((buf.width() as usize, buf.height() as usize), (SIDE, SIDE));
                assert!(*ms >= MIN_FRAME_MS);
            }
        }
    }

    #[test]
    fn frames_have_cat_pixels_and_transparent_corners() {
        let clip = decode(MUSIC[0]).unwrap();
        let (buf, _) = &clip.frames[0];
        let px = buf.as_bytes();
        assert_eq!(px[3], 0, "top-left corner should be transparent");
        assert!(px.chunks(4).any(|p| p[3] != 0), "frame should not be empty");
    }
}
