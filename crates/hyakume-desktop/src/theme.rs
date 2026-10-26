//! Light and dark palettes from the approved mockups. The app follows the
//! system appearance, which GPUI reads from the XDG desktop portal on Linux.

use gpui::{Hsla, WindowAppearance, rgb};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Hsla,
    pub dot: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    pub ink: Hsla,
    pub muted: Hsla,
    pub line: Hsla,
    pub web: Hsla,
    pub web_fill: Hsla,
    pub accent: Hsla,
    pub blocked: Hsla,
    pub on_blocked: Hsla,
    pub blocked_bg: Hsla,
    pub link: Hsla,
    pub live: Hsla,
    pub off: Hsla,
    pub eye: EyeColors,
    pub meme: MemeColors,
}

/// Meme's own palette, from the approved mascot sheet. Its body is dark in
/// both themes; the dark theme lifts it with a lighter rim and legs.
#[derive(Clone, Copy, Debug)]
pub struct MemeColors {
    pub body: Hsla,
    pub body_light: Hsla,
    pub body_deep: Hsla,
    pub rim: Option<Hsla>,
    pub gold: Hsla,
    pub gold_light: Hsla,
    pub gold_edge: Hsla,
    pub gold_deep: Hsla,
    pub socket: Hsla,
    pub pupil: Hsla,
    pub glint: Hsla,
    pub red: Hsla,
    pub hatch: Hsla,
    pub bone: Hsla,
    pub silk: Hsla,
}

/// Eyes have no whites on purpose: a bead, or a gold lantern with a slit.
#[derive(Clone, Copy, Debug)]
pub struct EyeColors {
    pub bead: Hsla,
    /// Light rim so a black bead still reads on a dark canvas.
    pub rim: Option<Hsla>,
    pub gold: Hsla,
    pub gold_rim: Hsla,
    pub pupil: Hsla,
    pub glint: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

impl Theme {
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    /// The colors an eye and its label are drawn with, at `alpha`. Find uses
    /// it to fade the tasks that don't match.
    pub fn faded(&self, alpha: f32) -> Self {
        let mut t = *self;
        for c in [
            &mut t.ink,
            &mut t.muted,
            &mut t.blocked,
            &mut t.on_blocked,
            &mut t.accent,
            &mut t.eye.bead,
            &mut t.eye.gold,
            &mut t.eye.gold_rim,
            &mut t.eye.pupil,
            &mut t.eye.glint,
        ] {
            *c = c.opacity(alpha);
        }
        t.eye.rim = t.eye.rim.map(|c| c.opacity(alpha));
        t
    }

    pub fn light() -> Self {
        Self {
            bg: c(0xF6F4EF),
            dot: c(0xE4DFD4),
            surface: c(0xFFFFFF),
            raised: c(0xFBFAF7),
            ink: c(0x1E1D1A),
            muted: c(0x625E55),
            line: c(0xDDD7CB),
            web: c(0xD3CCBE),
            web_fill: c(0xFFFFFF).opacity(0.6),
            accent: c(0xC2410C),
            blocked: c(0xB42318),
            on_blocked: c(0xFFFFFF),
            blocked_bg: c(0xFDECEA),
            link: c(0x3A3833),
            live: c(0x1F7A45),
            off: c(0xB42318),
            eye: EyeColors {
                bead: c(0x1E1D1A),
                rim: None,
                gold: c(0xF2A93B),
                gold_rim: c(0xC2410C),
                pupil: c(0x1E1D1A),
                glint: c(0xFFF8EC),
            },
            meme: MemeColors {
                body: c(0x2B231E),
                body_light: c(0x463931),
                body_deep: c(0x211A16),
                rim: None,
                gold: c(0xF2A93B),
                gold_light: c(0xFFD98C),
                gold_edge: c(0xE08A26),
                gold_deep: c(0xC2410C),
                socket: c(0x150F0C),
                pupil: c(0x1A120D),
                glint: c(0xFFF8EC),
                red: c(0xE4572E),
                hatch: c(0xFFB59C),
                bone: c(0xF3E3C6),
                silk: c(0xA39C8E),
            },
        }
    }

    pub fn dark() -> Self {
        Self {
            bg: c(0x101218),
            dot: c(0x1F2430),
            surface: c(0x171A21),
            raised: c(0x1D212A),
            ink: c(0xECEAE4),
            muted: c(0xA3A8B3),
            line: c(0x2E3340),
            web: c(0x343B4D),
            web_fill: c(0x171A21).opacity(0.55),
            accent: c(0xF2A93B),
            blocked: c(0xFF7A6E),
            on_blocked: c(0x1A0E0C),
            blocked_bg: c(0x2C1816),
            link: c(0xD5D8DE),
            live: c(0x5FD08A),
            off: c(0xFF7A6E),
            eye: EyeColors {
                bead: c(0x050608),
                rim: Some(c(0xA3A8B3)),
                gold: c(0xF2A93B),
                gold_rim: c(0xFFD38A),
                pupil: c(0x140E08),
                glint: c(0xFFF8EC),
            },
            meme: MemeColors {
                body: c(0x45382F),
                body_light: c(0x5C4C41),
                body_deep: c(0x3A2F28),
                rim: Some(c(0x86705E)),
                gold: c(0xF2A93B),
                gold_light: c(0xFFD98C),
                gold_edge: c(0xE08A26),
                gold_deep: c(0xC2560F),
                socket: c(0x1E1712),
                pupil: c(0x1A120D),
                glint: c(0xFFF8EC),
                red: c(0xF0643C),
                hatch: c(0xFFB59C),
                bone: c(0xF3E3C6),
                silk: c(0x8A90A0),
            },
        }
    }
}
