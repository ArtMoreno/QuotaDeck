//! Install the bundled brand-mark font where the terminal will find it.
//!
//! Terminals that use the system's font fallback (Terminal.app, iTerm2,
//! Ghostty, Kitty, Noctty) draw a private-use codepoint from any installed
//! font that has it. Other installed fonts share the `U+E1A0` run QuotaDeck
//! uses for most marks, but nothing else carries OpenRouter at `U+E500`, so
//! without this file that row draws a replacement box. WezTerm keeps its own
//! fallback list and still needs the configuration the README describes.
//!
//! Ghostty also gets a `font-codepoint-map` line pinning `U+E500` to this
//! font, the way herdr-radar pins its own range: with a map in place the
//! system fallback is skipped, so no other private-use font can claim the
//! codepoint first.
//!
//! The font is embedded so the installer never has to locate the checkout,
//! and only a file that is byte-for-byte this font is ever removed.
use crate::brand::GlyphSet;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const FONT_FILE: &str = "QuotaDeckIcons-Regular.ttf";
const FONT: &[u8] = include_bytes!("../../docs/icons/QuotaDeckIcons-Regular.ttf");
/// Family unquoted: Ghostty reads the name literally, quotes included.
const GHOSTTY_MAP: &str = "font-codepoint-map = U+E500=QuotaDeck Icons";

/// The per-user font directory the platform's font fallback scans. Windows
/// per-user fonts need a registry entry as well as the file, so the README
/// covers Windows terminals instead.
fn font_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let home = crate::platform::home_dir().ok()?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Fonts"));
    }
    Some(
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("fonts"),
    )
}

pub fn apply(glyphs: GlyphSet) -> Result<()> {
    if glyphs != GlyphSet::IconFont {
        return Ok(());
    }
    let Some(directory) = font_directory() else {
        return Ok(());
    };
    if install(&directory)? {
        refresh_font_cache(&directory);
        println!(
            "Installed the QuotaDeck Icons font to {}. Restart the terminal if a brand mark shows a box; WezTerm also needs the fallback entry from scripts/wezterm.lua.",
            directory.join(FONT_FILE).display()
        );
    }
    for config in ghostty_configs() {
        if map_codepoint(&config)? {
            println!(
                "Mapped U+E500 to QuotaDeck Icons in {}. Reload Ghostty's config or reopen it.",
                config.display()
            );
        }
    }
    Ok(())
}

/// Write the font unless the installed copy already is this font. Returns
/// whether anything was written.
fn install(directory: &Path) -> Result<bool> {
    let path = directory.join(FONT_FILE);
    if fs::read(&path).is_ok_and(|current| current == FONT) {
        return Ok(false);
    }
    fs::create_dir_all(directory)
        .with_context(|| format!("create font directory {}", directory.display()))?;
    fs::write(&path, FONT).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Linux terminals read fontconfig's cache; macOS notices new files itself.
fn refresh_font_cache(directory: &Path) {
    if cfg!(target_os = "linux") {
        let _ = std::process::Command::new("fc-cache")
            .arg("-f")
            .arg(directory)
            .output();
    }
}

pub fn uninstall() -> Result<()> {
    let Some(directory) = font_directory() else {
        return Ok(());
    };
    for config in ghostty_configs() {
        unmap_codepoint(&config)?;
    }
    remove(&directory)
}

fn remove(directory: &Path) -> Result<()> {
    let path = directory.join(FONT_FILE);
    if fs::read(&path).is_ok_and(|current| current == FONT) {
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Every Ghostty config file that already exists. Ghostty reads `config` and
/// `config.ghostty` from `$XDG_CONFIG_HOME/ghostty` and, on macOS, from its
/// Application Support directory too. None is ever created: without one,
/// Ghostty is either absent or running on defaults the user never chose to
/// keep in a file.
fn ghostty_configs() -> Vec<PathBuf> {
    let Ok(home) = crate::platform::home_dir() else {
        return Vec::new();
    };
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let mut directories = vec![xdg.join("ghostty")];
    if cfg!(target_os = "macos") {
        directories.push(home.join("Library/Application Support/com.mitchellh.ghostty"));
    }
    directories
        .iter()
        .flat_map(|directory| ["config", "config.ghostty"].map(|name| directory.join(name)))
        .filter(|path| path.is_file())
        .collect()
}

fn is_map_line(line: &str) -> bool {
    line.trim() == GHOSTTY_MAP
}

/// Append the codepoint map unless the config already has it. Returns whether
/// anything was written.
fn map_codepoint(config: &Path) -> Result<bool> {
    let text = fs::read_to_string(config).with_context(|| format!("read {}", config.display()))?;
    if text.lines().any(is_map_line) {
        return Ok(false);
    }
    let mut next = text;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(GHOSTTY_MAP);
    next.push('\n');
    fs::write(config, next).with_context(|| format!("write {}", config.display()))?;
    Ok(true)
}

/// Drop only the codepoint map line; everything else, other plugins' maps
/// included, stays byte-for-byte.
fn unmap_codepoint(config: &Path) -> Result<()> {
    let text = fs::read_to_string(config).with_context(|| format!("read {}", config.display()))?;
    let next: String = text
        .split_inclusive('\n')
        .filter(|line| !is_map_line(line))
        .collect();
    if next != text {
        fs::write(config, next).with_context(|| format!("write {}", config.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_is_installed_once_updated_when_stale_and_removed_only_when_ours() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("fonts");
        assert!(install(&directory).unwrap());
        assert!(
            !install(&directory).unwrap(),
            "unchanged copy is left alone"
        );
        let path = directory.join(FONT_FILE);
        assert_eq!(fs::read(&path).unwrap(), FONT);

        fs::write(&path, b"an older QuotaDeck Icons build").unwrap();
        assert!(install(&directory).unwrap(), "a stale copy is replaced");
        assert_eq!(fs::read(&path).unwrap(), FONT);

        remove(&directory).unwrap();
        assert!(!path.exists());
        fs::write(&path, b"someone else's file with our name").unwrap();
        remove(&directory).unwrap();
        assert!(
            path.exists(),
            "a file that is not this font is never removed"
        );
    }

    #[test]
    fn the_ghostty_map_is_added_once_and_removed_without_touching_other_lines() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let radar = "font-size = 18\n# >>> herdr-radar font block\nfont-codepoint-map = U+E1A0-U+E1B7=Herdr Agent Icons Max\n# <<< herdr-radar font block";
        fs::write(&config, radar).unwrap();

        assert!(map_codepoint(&config).unwrap());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            format!("{radar}\n{GHOSTTY_MAP}\n"),
            "appended on its own line after a file with no final newline"
        );
        assert!(
            !map_codepoint(&config).unwrap(),
            "an existing map is left alone"
        );

        // A map the user placed by hand elsewhere counts as present.
        let by_hand = format!("{GHOSTTY_MAP}\n{radar}\n");
        fs::write(&config, &by_hand).unwrap();
        assert!(!map_codepoint(&config).unwrap());

        unmap_codepoint(&config).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), format!("{radar}\n"));
        unmap_codepoint(&config).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), format!("{radar}\n"));
    }

    #[test]
    fn the_embedded_font_carries_every_icon_font_codepoint() {
        // A TrueType file starts with the 0x00010000 tag; the cmap is checked
        // by the build script, this only guards against an empty embed.
        assert!(FONT.len() > 4_000, "{}", FONT.len());
        assert_eq!(&FONT[..4], &[0, 1, 0, 0]);
    }
}
