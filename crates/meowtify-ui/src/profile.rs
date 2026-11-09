//! The pseudo profile: a name such as "Sleepy Otter" and a cat of its own colour.
//!
//! There is no account. The name is the whole profile, and the colour comes from the name, so
//! everyone in a jam sees the same cat for the same name without sending anything but the name.
//! A user may pick a colour instead, and then the hue is sent along with the name.
//! The cat is the app mascot with its body recoloured. The headphones, eyes and highlights keep
//! their colours.

use std::cell::OnceCell;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

static MASCOT: &[u8] = include_bytes!("../../../assets/icon/meowtify-orange-512.png");

/// Side of an avatar in pixels. The mascot is a 16 by 16 grid, so 64 is four pixels per cell.
const SIDE: usize = 64;

/// The orange body and its shade, as they are drawn in the mascot file.
const BODY: [u8; 3] = [242, 158, 76];
const SHADE: [u8; 3] = [217, 130, 58];

const ADJECTIVES: [&str; 48] = [
    "Sleepy", "Fuzzy", "Brave", "Sneaky", "Jolly", "Cosmic", "Mellow", "Sparkly", "Grumpy", "Cheeky", "Dizzy", "Gentle",
    "Lucky", "Misty", "Nimble", "Plucky", "Quiet", "Rowdy", "Silly", "Tiny", "Velvet", "Wobbly", "Zesty", "Breezy",
    "Cozy", "Dreamy", "Electric", "Fluffy", "Giddy", "Happy", "Icy", "Jazzy", "Keen", "Lazy", "Merry", "Noisy",
    "Odd", "Peppy", "Quirky", "Rusty", "Shiny", "Toasty", "Unruly", "Vivid", "Witty", "Young", "Zany", "Moonlit",
];

const ANIMALS: [&str; 48] = [
    "Otter", "Panda", "Fox", "Owl", "Badger", "Koala", "Llama", "Gecko", "Heron", "Ferret", "Walrus", "Newt",
    "Moose", "Lynx", "Finch", "Crab", "Bat", "Beaver", "Bison", "Camel", "Dingo", "Eagle", "Frog", "Goat",
    "Hare", "Ibis", "Jackal", "Kiwi", "Lemur", "Mole", "Narwhal", "Ocelot", "Parrot", "Quokka", "Raven", "Seal",
    "Tapir", "Urchin", "Viper", "Wombat", "Yak", "Zebra", "Alpaca", "Bunny", "Duck", "Hedgehog", "Penguin", "Sloth",
];

/// A random name of two words, such as "Sleepy Otter". `None` when the system gives no random
/// numbers.
pub(crate) fn random_name() -> Option<String> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).ok()?;
    let n = u32::from_le_bytes(bytes) as usize;
    // Different parts of the number pick each word, so the pair is not tied together.
    Some(format!("{} {}", ADJECTIVES[n % ADJECTIVES.len()], ANIMALS[(n / ADJECTIVES.len()) % ANIMALS.len()]))
}

/// The cat for a name. With no chosen `hue` (degrees), the same name always gives the same colours.
pub(crate) fn avatar(name: &str, hue: Option<u16>) -> Image {
    Image::from_rgba8(recolor(base(), hue.map_or_else(|| hue_of(name), f32::from)))
}

/// The hues the colour picker offers, in degrees.
pub(crate) const PALETTE: [u16; 12] = [0, 25, 45, 80, 130, 165, 190, 215, 245, 275, 305, 335];

/// The swatch colour for a hue, the same as the cat's fur.
pub(crate) fn swatch(hue: u16) -> slint::Color {
    let [r, g, b] = hsl(f32::from(hue), 0.70, 0.65);
    slint::Color::from_rgb_u8(r, g, b)
}

/// The colour of a person's cat: the chosen hue, else the one their name gives.
pub(crate) fn color(name: &str, hue: Option<u16>) -> slint::Color {
    let [r, g, b] = hsl(hue.map_or_else(|| hue_of(name), f32::from), 0.70, 0.65);
    slint::Color::from_rgb_u8(r, g, b)
}

/// A hue in degrees. FNV-1a over the lower case name; hashing spreads similar names apart.
fn hue_of(name: &str) -> f32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.trim().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h % 360) as f32
}

/// HSL to RGB, with saturation and lightness from 0 to 1.
fn hsl(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

thread_local! {
    static BASE: OnceCell<SharedPixelBuffer<Rgba8Pixel>> = const { OnceCell::new() };
}

/// The mascot at `SIDE` pixels, decoded once.
fn base() -> SharedPixelBuffer<Rgba8Pixel> {
    BASE.with(|b| b.get_or_init(|| decode().unwrap_or_else(|| SharedPixelBuffer::new(SIDE as u32, SIDE as u32))).clone())
}

fn decode() -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(MASCOT)).read_info().ok()?;
    let mut raw = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut raw).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let (w, h) = (info.width as usize, info.height as usize);
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(SIDE as u32, SIDE as u32);
    let pixels = out.make_mut_slice();
    for y in 0..SIDE {
        for x in 0..SIDE {
            // The middle of each block, so a cell's colour is taken from inside it.
            let (sx, sy) = ((x * 2 + 1) * w / (SIDE * 2), (y * 2 + 1) * h / (SIDE * 2));
            let at = (sy * w + sx) * 4;
            pixels[y * SIDE + x] = Rgba8Pixel { r: raw[at], g: raw[at + 1], b: raw[at + 2], a: raw[at + 3] };
        }
    }
    Some(out)
}

fn recolor(mut img: SharedPixelBuffer<Rgba8Pixel>, hue: f32) -> SharedPixelBuffer<Rgba8Pixel> {
    let body = hsl(hue, 0.70, 0.65);
    let shade = hsl(hue, 0.62, 0.53);
    for p in img.make_mut_slice() {
        let rgb = [p.r, p.g, p.b];
        if p.a == 255 && rgb == BODY {
            [p.r, p.g, p.b] = body;
        } else if p.a == 255 && rgb == SHADE {
            [p.r, p.g, p.b] = shade;
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(img: &SharedPixelBuffer<Rgba8Pixel>) -> Vec<[u8; 4]> {
        img.as_slice().iter().map(|p| [p.r, p.g, p.b, p.a]).collect()
    }

    #[test]
    fn the_mascot_decodes_to_a_small_square() {
        let img = base();
        assert_eq!((img.width(), img.height()), (SIDE as u32, SIDE as u32));
        let px = pixels(&img);
        assert!(px.iter().any(|p| p[..3] == BODY && p[3] == 255), "body colour found");
        assert!(px.iter().any(|p| p[3] == 0), "transparent corners found");
    }

    #[test]
    fn only_the_fur_changes_colour() {
        let before = pixels(&base());
        let after = pixels(&recolor(base(), 200.0));
        let mut fur = 0;
        for (a, b) in before.iter().zip(&after) {
            if a[..3] == BODY || a[..3] == SHADE {
                if a[3] == 255 {
                    fur += 1;
                    assert_ne!(a, b, "fur is recoloured");
                }
            } else {
                assert_eq!(a, b, "headphones, eyes and the background keep their colour");
            }
        }
        assert!(fur > 500, "{fur}");
    }

    #[test]
    fn a_name_always_has_the_same_cat() {
        assert_eq!(hue_of("Sleepy Otter"), hue_of("  sleepy otter "));
        assert_ne!(hue_of("Sleepy Otter"), hue_of("Brave Fox"));
        assert_eq!(pixels(&recolor(base(), hue_of("Brave Fox"))), pixels(&recolor(base(), hue_of("Brave Fox"))));
    }

    #[test]
    fn hsl_hits_known_colours() {
        assert_eq!(hsl(0.0, 1.0, 0.5), [255, 0, 0]);
        assert_eq!(hsl(120.0, 1.0, 0.5), [0, 255, 0]);
        assert_eq!(hsl(240.0, 1.0, 0.5), [0, 0, 255]);
        assert_eq!(hsl(100.0, 0.0, 0.5), [128, 128, 128]);
    }

    #[test]
    fn names_are_an_adjective_and_an_animal() {
        let name = random_name().unwrap();
        let (adj, animal) = name.split_once(' ').unwrap();
        assert!(ADJECTIVES.contains(&adj) && ANIMALS.contains(&animal), "{name}");
    }
}
