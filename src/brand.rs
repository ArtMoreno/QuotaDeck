//! Brand marks for the sidebar.
//!
//! Herdr's sidebar is a grid of text cells, so a "logo" here is a glyph, not an
//! image. Which glyph actually renders depends entirely on the terminal's font,
//! and a codepoint the font does not carry draws as a tofu box — worse than no
//! mark at all. So the set is selectable rather than assumed, and the default
//! is the one this plugin can point at a real font for.
//!
//! The repository also ships SVG and PNG logos under `docs/icons/`. Those
//! are for the README and for any surface that can display an image; they
//! cannot appear in the sidebar, and nothing here reads them.

use crate::model::Provider;

/// Which family of marks to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GlyphSet {
    /// Private-use codepoints from the `Herdr Agent Icons Max` font, the same
    /// U+E1A0.. block the `herdr-icon-agent-ui` plugin defines. These are real
    /// brand logos and are the reason this is the default: they are the only
    /// set with a font that can actually be installed for them.
    #[default]
    IconFont,
    /// Widely-supported Unicode marks. Not brand logos, but they render in
    /// any monospace font, including the stock terminal ones.
    Unicode,
    /// No mark. The provider name alone, exactly as upstream renders it.
    Off,
}

impl GlyphSet {
    pub const ENV: &'static str = "HERDR_AGENT_QUOTA_BRAND_GLYPHS";

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "icon" | "iconfont" | "icon-font" | "on" => Some(Self::IconFont),
            "unicode" | "safe" => Some(Self::Unicode),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IconFont => "icon",
            Self::Unicode => "unicode",
            Self::Off => "off",
        }
    }

    /// The next value when the settings pane cycles this control.
    pub fn next(self) -> Self {
        match self {
            Self::IconFont => Self::Unicode,
            Self::Unicode => Self::Off,
            Self::Off => Self::IconFont,
        }
    }

    /// The mark for a provider, or `None` when nothing should be drawn.
    ///
    /// A provider with no logo in the set returns `None` rather than a generic
    /// stand-in: one unlabelled box among real logos reads as a rendering bug.
    pub fn glyph(self, provider: Provider) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::IconFont => icon_font_glyph(provider),
            Self::Unicode => unicode_glyph(provider),
        }
    }

    /// Prefix a label with the provider's mark.
    ///
    /// The separator is a single space: Herdr splits sidebar tokens on
    /// whitespace for wrapping, and a wider gap lets a row break between the
    /// logo and the name it belongs to.
    pub fn label(self, provider: Provider, text: &str) -> String {
        match self.glyph(provider) {
            Some(glyph) if !text.is_empty() => format!("{glyph} {text}"),
            Some(glyph) => glyph.to_string(),
            None => text.to_string(),
        }
    }
}

/// `Herdr Agent Icons Max` private-use codepoints.
///
/// Kept byte-for-byte aligned with that font's `codepoints.toml`. Agy borrows
/// the Gemini mark because Antigravity is Google's harness and the font has no
/// separate Antigravity logo.
///
/// OpenRouter is not in that font, so QuotaDeck's own font carries it. It
/// lives at U+E500 rather than the next slot after Grok: the upstream font
/// hands out codepoints sequentially from U+E1A0, so any slot next to its
/// run is claimed by whatever logo it adds next, and a terminal that loads
/// the upstream font first then draws that logo instead of OpenRouter's.
/// U+E500 is also outside every Nerd Fonts block.
fn icon_font_glyph(provider: Provider) -> Option<&'static str> {
    Some(match provider {
        Provider::Claude => "\u{E1A0}",
        Provider::Codex => "\u{E1A1}",
        Provider::OpenCodeGo => "\u{E1A2}",
        Provider::Omp => "\u{E1A3}",
        Provider::Hermes => "\u{E1AA}",
        Provider::Grok => "\u{E1B1}",
        Provider::Agy => "\u{E1AE}",
        Provider::OpenRouter => "\u{E500}",
    })
}

/// Marks from ranges a stock monospace font carries.
///
/// These are mnemonics, not logos. The point is that they always draw, which
/// rules out two things a nicer-looking set would use. Nothing here may be
/// East-Asian *Ambiguous* width — under a CJK locale, or Windows Terminal with
/// wide-ambiguous on, such a mark takes two cells while its neighbours take one
/// and the sidebar grid stops lining up. And nothing may sit in a block the
/// stock terminal fonts skip: U+2301 ELECTRIC ARROW reads well and is absent
/// from Consolas, Cascadia Code, DejaVu Sans Mono and Menlo alike.
fn unicode_glyph(provider: Provider) -> Option<&'static str> {
    Some(match provider {
        Provider::Claude => "✳",
        Provider::Codex => "◉",
        Provider::Grok => "✕",
        Provider::Agy => "✦",
        Provider::OpenCodeGo => "❑",
        Provider::Omp => "⬢",
        Provider::Hermes => "❉",
        // A router: many inputs, one outbound path.
        Provider::OpenRouter => "⇄",
    })
}

/// Logo asset shipped in `docs/icons/`, for surfaces that can show an image.
///
/// Not used by the sidebar. Exposed so the README check and any future
/// image-capable surface agree on one filename per provider.
pub fn asset_stem(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Grok => "grok",
        Provider::Agy => "agy",
        Provider::OpenCodeGo => "opencode",
        Provider::Omp => "omp",
        Provider::Hermes => "hermes",
        Provider::OpenRouter => "openrouter",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_set_is_the_one_with_an_installable_font() {
        assert_eq!(GlyphSet::default(), GlyphSet::IconFont);
    }

    #[test]
    fn off_draws_nothing_and_leaves_the_label_alone() {
        assert_eq!(GlyphSet::Off.glyph(Provider::Claude), None);
        assert_eq!(GlyphSet::Off.label(Provider::Claude, "Claude"), "Claude");
    }

    #[test]
    fn openrouter_uses_its_installed_brand_glyph() {
        assert_eq!(
            GlyphSet::IconFont.glyph(Provider::OpenRouter),
            Some("\u{E500}")
        );
        assert_eq!(
            GlyphSet::IconFont.label(Provider::OpenRouter, "OpenRouter"),
            "\u{E500} OpenRouter"
        );
    }

    #[test]
    fn the_unicode_set_covers_every_provider() {
        // The fallback exists precisely so nothing is ever left unmarked.
        for provider in [
            Provider::Claude,
            Provider::Codex,
            Provider::Grok,
            Provider::Agy,
            Provider::OpenCodeGo,
            Provider::Omp,
            Provider::Hermes,
            Provider::OpenRouter,
        ] {
            assert!(GlyphSet::Unicode.glyph(provider).is_some(), "{provider:?}");
        }
    }

    #[test]
    fn icon_font_codepoints_match_the_font_table() {
        // Aligned with Herdr Agent Icons Max codepoints.toml.
        assert_eq!(GlyphSet::IconFont.glyph(Provider::Claude), Some("\u{E1A0}"));
        assert_eq!(GlyphSet::IconFont.glyph(Provider::Hermes), Some("\u{E1AA}"));
        assert_eq!(GlyphSet::IconFont.glyph(Provider::Grok), Some("\u{E1B1}"));
        assert_eq!(
            GlyphSet::IconFont.glyph(Provider::OpenRouter),
            Some("\u{E500}")
        );
    }

    #[test]
    fn the_label_separator_is_one_space() {
        assert_eq!(
            GlyphSet::Unicode.label(Provider::Claude, "Claude"),
            "✳ Claude"
        );
    }

    #[test]
    fn names_round_trip_and_cycle() {
        for set in [GlyphSet::IconFont, GlyphSet::Unicode, GlyphSet::Off] {
            assert_eq!(GlyphSet::parse(set.as_str()), Some(set));
        }
        assert_eq!(GlyphSet::IconFont.next().next().next(), GlyphSet::IconFont);
    }

    #[test]
    fn every_documented_mark_ships_as_svg_and_png() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/icons");
        for stem in [
            "agy",
            "claude",
            "codex",
            "cursor",
            "grok",
            "hermes",
            "omp",
            "opencode",
            "openrouter",
            "pi",
        ] {
            for extension in ["svg", "png"] {
                assert!(
                    root.join(format!("{stem}.{extension}")).is_file(),
                    "missing {stem}.{extension}"
                );
            }
            if stem == "hermes" {
                use sha2::Digest;

                let png = std::fs::read(root.join("hermes.png")).unwrap();
                assert_eq!(
                    format!("{:x}", sha2::Sha256::digest(png)),
                    "d17f5e65d618e54e74d98ff5c8b59c5242c80a77afbc8f96fa05bea61e963a87",
                    "regenerate hermes.png from its SVG with transparent corners"
                );
            }
        }
    }
}
