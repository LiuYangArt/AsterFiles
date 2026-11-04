//! Accent-only tokens; unrelated surfaces and text remain unchanged (#173).
pub(super) const LIGHT_DEFAULT: u32 = 0xff4f_6bed;
const DARK_DEFAULT: u32 = 0xff7f_a0ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Palette {
    pub accent: u32,
    pub selection: u32,
    pub emphasis: u32,
    pub hover: u32,
    pub rectangle_fill: u32,
    pub rectangle_border: u32,
}

impl Palette {
    pub fn derive(system: Option<u32>, dark: bool) -> Self {
        let Some(raw) = system else {
            return if dark {
                Self {
                    accent: DARK_DEFAULT,
                    selection: 0xff26_3d66,
                    emphasis: 0xff35_548f,
                    hover: 0xff30_3b4d,
                    rectangle_fill: 0x1a7f_a0ff,
                    rectangle_border: 0x997f_a0ff,
                }
            } else {
                Self {
                    accent: LIGHT_DEFAULT,
                    selection: 0xffdc_e6ff,
                    emphasis: 0xffc7_d7ff,
                    hover: 0xffef_f4ff,
                    rectangle_fill: 0x1a4f_6bed,
                    rectangle_border: 0x994f_6bed,
                }
            };
        };
        let raw = raw | 0xff00_0000;
        let surface = if dark { 0xff25_2525 } else { 0xffff_ffff };
        let text = if dark { 0xffbd_bdbd } else { 0xff6f_717a };
        let endpoint = if dark { 0xffff_ffff } else { 0xff00_0000 };
        // Loading text also appears on active navigation tabs and hovered title-bar tabs.
        let accent_surface = if dark { 0xff3d_3d3d } else { 0xffe9_e9e9 };
        let accent = ensure_contrast(raw, accent_surface, endpoint);
        let background = |weight| ensure_contrast(mix(surface, raw, weight), text, surface);
        Self {
            accent,
            selection: background(if dark { 0.22 } else { 0.16 }),
            emphasis: background(if dark { 0.32 } else { 0.28 }),
            hover: background(if dark { 0.12 } else { 0.08 }),
            rectangle_fill: (accent & 0x00ff_ffff) | 0x1a00_0000,
            rectangle_border: (accent & 0x00ff_ffff) | 0x9900_0000,
        }
    }
}

fn mix(from: u32, to: u32, weight: f64) -> u32 {
    let channel = |shift: u32| {
        let a = f64::from((from >> shift) & 255_u32);
        let b = f64::from((to >> shift) & 255_u32);
        ((a + (b - a) * weight).round() as u32) << shift
    };
    0xff00_0000_u32 | channel(16) | channel(8) | channel(0)
}

fn luminance(color: u32) -> f64 {
    let linear = |shift: u32| {
        let value = f64::from((color >> shift) & 255_u32) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

fn contrast(a: u32, b: u32) -> f64 {
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn ensure_contrast(color: u32, against: u32, endpoint: u32) -> u32 {
    // Keep the hue until WCAG's normal-text threshold requires moving toward the theme endpoint.
    for step in 0..=100 {
        let candidate = mix(color, endpoint, f64::from(step) / 100.0);
        if contrast(candidate, against) >= 4.5 {
            return candidate;
        }
    }
    endpoint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_173_read_failure_keeps_previous_theme_defaults() {
        assert_eq!(
            Palette::derive(None, false),
            Palette {
                accent: LIGHT_DEFAULT,
                selection: 0xffdc_e6ff,
                emphasis: 0xffc7_d7ff,
                hover: 0xffef_f4ff,
                rectangle_fill: 0x1a4f_6bed,
                rectangle_border: 0x994f_6bed,
            }
        );
        assert_eq!(
            Palette::derive(None, true),
            Palette {
                accent: DARK_DEFAULT,
                selection: 0xff26_3d66,
                emphasis: 0xff35_548f,
                hover: 0xff30_3b4d,
                rectangle_fill: 0x1a7f_a0ff,
                rectangle_border: 0x997f_a0ff,
            }
        );
    }

    #[test]
    fn issue_173_extreme_and_saturated_accents_preserve_text_contrast() {
        for dark in [false, true] {
            let surface = if dark { 0xff25_2525 } else { 0xffff_ffff };
            let text = if dark { 0xffbd_bdbd } else { 0xff6f_717a };
            for r in (0..=255).step_by(51) {
                for g in (0..=255).step_by(51) {
                    for b in (0..=255).step_by(51) {
                        let palette =
                            Palette::derive(Some(0xff00_0000 | r << 16 | g << 8 | b), dark);
                        for background in [
                            surface,
                            if dark { 0xff32_3437 } else { 0xffe9_e9e9 },
                            if dark { 0xff3d_3d3d } else { 0xffe8_eaf0 },
                        ] {
                            assert!(contrast(palette.accent, background) >= 4.5);
                        }
                        for background in [palette.selection, palette.emphasis, palette.hover] {
                            assert!(contrast(background, text) >= 4.5);
                        }
                        assert_eq!(palette.rectangle_fill >> 24, 0x1a);
                        assert_eq!(palette.rectangle_border >> 24, 0x99);
                        assert_eq!(palette.rectangle_fill & 0xffffff, palette.accent & 0xffffff);
                    }
                }
            }
        }
    }

    #[test]
    fn issue_173_derived_tokens_change_with_system_color_and_theme() {
        for dark in [false, true] {
            let blue = Palette::derive(Some(0xff00_78d4), dark);
            let red = Palette::derive(Some(0xffc4_2b1c), dark);
            assert_ne!(blue.accent, red.accent);
            assert_ne!(blue.selection, red.selection);
            assert_ne!(blue.emphasis, red.emphasis);
            assert_ne!(blue.hover, red.hover);
        }
        assert_ne!(
            Palette::derive(Some(0xff00_78d4), false),
            Palette::derive(Some(0xff00_78d4), true)
        );
    }
}
