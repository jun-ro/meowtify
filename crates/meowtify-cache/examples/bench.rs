//! cargo run --release -p meowtify-cache --example bench
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

struct Counting;
static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let c = CUR.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(c, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        CUR.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = System.realloc(p, l, n);
        if !q.is_null() {
            if n >= l.size() {
                let c = CUR.fetch_add(n - l.size(), Relaxed) + (n - l.size());
                PEAK.fetch_max(c, Relaxed);
            } else {
                CUR.fetch_sub(l.size() - n, Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn main() {
    // Photo-like content: smooth gradients plus deterministic noise
    // (flat images encode unrealistically small and fast).
    let mut seed = 12345u32;
    let img = image::RgbImage::from_fn(640, 640, |x, y| {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let n = (seed >> 27) as i32 - 16;
        let f = |v: i32| (v + n).clamp(0, 255) as u8;
        image::Rgb([
            f(((x as f32 / 40.0).sin() * 90.0 + 128.0) as i32),
            f(((y as f32 / 33.0).cos() * 90.0 + 128.0) as i32),
            f(((x + y) / 5 % 256) as i32),
        ])
    });
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 85).encode_image(&img).unwrap();
    drop(img);
    println!("source jpeg: {} KiB", jpg.len() / 1024);
    for side in [64u32, 128] {
        let _ = meowtify_cache::decode_square(&jpg, side).unwrap(); // warm-up
        let n = 50;
        let base = CUR.load(Relaxed);
        PEAK.store(base, Relaxed);
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(meowtify_cache::decode_square(&jpg, side).unwrap());
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!(
            "cover 640x640 -> {side}x{side}: {ms:.2} ms/image, peak extra heap {:.2} MiB",
            (PEAK.load(Relaxed) - base) as f64 / 1048576.0
        );
    }
}
