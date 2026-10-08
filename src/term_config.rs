//! Terminal settings from `~/.config/seance/terminal.conf`.
//!
//! The file uses Ghostty's config syntax so a Ghostty user can name a theme
//! or include their existing config (`config-file = ~/.config/ghostty/config`)
//! instead of copying values over. Only the keys below are read; everything
//! else is ignored. No file means the built-in look, unchanged.
//!
//! Read once per process. The daemon resolves cell colors from the palette;
//! the GUI uses the default fg/bg, the font, the bell sound and the
//! command-finish rule. Each machine reads its own file, so a thin client and
//! its daemon each need one.
//!
//! | key | used by |
//! |-----|---------|
//! | `theme` | Ghostty theme name or path (palette/fg/bg/cursor) |
//! | `palette`, `background`, `foreground`, `cursor-color` | colors, override the theme |
//! | `font-family`, `font-size` (points, like Ghostty) | GUI font |
//! | `bell-features` (`audio`), `bell-audio-path`, `bell-audio-volume` | sound on BEL |
//! | `notify-on-command-finish` (`never`/`unfocused`/`always`), `notify-on-command-finish-after` | shell command alerts |
//! | `config-file` | include another file (`?` prefix = optional) |

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The palette seance shipped with (base16 default), kept as the fallback.
const DEFAULT_PALETTE: [u32; 16] = [
    0x00_18_18_18, //  0 black
    0x00_ab_46_42, //  1 red
    0x00_a1_b5_6c, //  2 green
    0x00_f7_ca_88, //  3 yellow
    0x00_7c_af_c2, //  4 blue
    0x00_ba_8b_af, //  5 magenta
    0x00_86_c1_b9, //  6 cyan
    0x00_d8_d8_d8, //  7 white
    0x00_58_58_58, //  8 bright black
    0x00_ab_46_42, //  9 bright red
    0x00_a1_b5_6c, // 10 bright green
    0x00_f7_ca_88, // 11 bright yellow
    0x00_7c_af_c2, // 12 bright blue
    0x00_ba_8b_af, // 13 bright magenta
    0x00_86_c1_b9, // 14 bright cyan
    0x00_f8_f8_f8, // 15 bright white
];

/// Include depth cap, so a config-file loop can't recurse forever.
const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandFinishNotify {
    Never,
    Unfocused,
    Always,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TermConfig {
    pub palette: [u32; 16],
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub font_family: Option<String>,
    /// Points, as Ghostty means them. See [`crate::term_font::font_size`].
    pub font_size: Option<f32>,
    /// Only played when `bell-features` includes `audio`, as in Ghostty.
    pub bell_audio_path: Option<PathBuf>,
    pub bell_audio: bool,
    pub bell_audio_volume: f32,
    pub notify_on_command_finish: CommandFinishNotify,
    pub notify_on_command_finish_after_ms: u64,
}

impl Default for TermConfig {
    fn default() -> Self {
        Self {
            palette: DEFAULT_PALETTE,
            foreground: 0x00_d8_d8_d8,
            background: 0x00_18_18_18,
            cursor: 0x00_e5_c0_7b,
            font_family: None,
            font_size: None,
            bell_audio_path: None,
            bell_audio: false,
            bell_audio_volume: 1.0,
            notify_on_command_finish: CommandFinishNotify::Never,
            notify_on_command_finish_after_ms: 5_000,
        }
    }
}

pub fn get() -> &'static TermConfig {
    static CONFIG: OnceLock<TermConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let path = PathBuf::from(shellexpand::tilde("~/.config/seance/terminal.conf").as_ref());
        load(&path)
    })
}

fn load(path: &Path) -> TermConfig {
    let mut entries = Vec::new();
    collect_entries(path, 0, &mut entries);
    let mut cfg = TermConfig::default();
    // Ghostty applies the theme first, so explicit colors in the config win.
    if let Some((_, theme)) = entries.iter().rev().find(|(k, _)| k == "theme") {
        // `light:X,dark:Y` picks dark: the terminal ground is always dark.
        let theme = theme
            .split(',')
            .find_map(|t| t.trim().strip_prefix("dark:"))
            .unwrap_or(theme);
        if let Some(theme_path) = find_theme(theme) {
            let mut theme_entries = Vec::new();
            collect_entries(&theme_path, MAX_INCLUDE_DEPTH, &mut theme_entries);
            for (k, v) in &theme_entries {
                apply(&mut cfg, k, v);
            }
        }
    }
    for (k, v) in &entries {
        apply(&mut cfg, k, v);
    }
    cfg
}

/// `key = value` pairs from `path` in order, with `config-file` includes
/// appended after the file's own lines (Ghostty's include order).
fn collect_entries(path: &Path, depth: usize, out: &mut Vec<(String, String)>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let base = path.parent().unwrap_or(Path::new("."));
    let mut includes = Vec::new();
    for (key, value) in parse_lines(&text) {
        match key.as_str() {
            "config-file" => includes.push(value),
            // Relative paths mean "next to this file", as in Ghostty.
            "bell-audio-path" if !value.is_empty() => {
                let p = PathBuf::from(shellexpand::tilde(&value).as_ref());
                out.push((key, base.join(p).to_string_lossy().into_owned()));
            }
            _ => out.push((key, value)),
        }
    }
    if depth >= MAX_INCLUDE_DEPTH {
        return;
    }
    for inc in includes {
        let inc = unquote(inc.trim_start_matches('?'));
        let inc = PathBuf::from(shellexpand::tilde(inc).as_ref());
        collect_entries(&base.join(inc), depth + 1, out);
    }
}

fn parse_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_string(), unquote(v.trim()).to_string()))
        })
        .collect()
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

fn apply(cfg: &mut TermConfig, key: &str, value: &str) {
    match key {
        "palette" => {
            if let Some((idx, color)) = value.split_once('=') {
                if let (Ok(idx), Some(color)) = (idx.trim().parse::<usize>(), parse_color(color)) {
                    if idx < 16 {
                        cfg.palette[idx] = color;
                    }
                }
            }
        }
        "background" => set_color(&mut cfg.background, value),
        "foreground" => set_color(&mut cfg.foreground, value),
        "cursor-color" => set_color(&mut cfg.cursor, value),
        // Ghostty: the first font-family is the font, later ones are
        // fallbacks, and an empty value clears the list.
        "font-family" if value.is_empty() => cfg.font_family = None,
        "font-family" if cfg.font_family.is_none() => cfg.font_family = Some(value.to_string()),
        "font-size" => {
            if let Some(size) = value.parse().ok().filter(|s: &f32| *s > 0.0) {
                cfg.font_size = Some(size);
            }
        }
        "bell-audio-path" if !value.is_empty() => cfg.bell_audio_path = Some(PathBuf::from(value)),
        "bell-features" => {
            for feature in value.split(',').map(str::trim) {
                match feature {
                    "audio" => cfg.bell_audio = true,
                    "no-audio" => cfg.bell_audio = false,
                    _ => {}
                }
            }
        }
        "bell-audio-volume" => {
            if let Ok(v) = value.parse::<f32>() {
                cfg.bell_audio_volume = v.clamp(0.0, 1.0);
            }
        }
        "notify-on-command-finish" => {
            cfg.notify_on_command_finish = match value {
                "unfocused" => CommandFinishNotify::Unfocused,
                "always" => CommandFinishNotify::Always,
                _ => CommandFinishNotify::Never,
            }
        }
        "notify-on-command-finish-after" => {
            if let Some(ms) = parse_duration_ms(value) {
                cfg.notify_on_command_finish_after_ms = ms;
            }
        }
        _ => {}
    }
}

fn set_color(slot: &mut u32, value: &str) {
    if let Some(c) = parse_color(value) {
        *slot = c;
    }
}

/// `#rrggbb` or `rrggbb`. Ghostty also takes X11 color names; those are skipped.
fn parse_color(value: &str) -> Option<u32> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Ghostty durations like `5s`, `500ms`, `1m`, or `1m30s`.
fn parse_duration_ms(value: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut rest = value.trim();
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let n: u64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let mult = match &rest[..unit_len] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => return None,
        };
        total += n * mult;
        rest = &rest[unit_len..];
    }
    Some(total)
}

/// A theme name resolves against Ghostty's theme folders; a path is used as-is.
fn find_theme(theme: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(shellexpand::tilde(theme).as_ref());
    if direct.is_absolute() {
        return direct.is_file().then_some(direct);
    }
    let mut dirs = vec![PathBuf::from(
        shellexpand::tilde("~/.config/ghostty/themes").as_ref(),
    )];
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("ghostty/themes")),
    );
    dirs.push(PathBuf::from(
        "/Applications/Ghostty.app/Contents/Resources/ghostty/themes",
    ));
    dirs.into_iter()
        .map(|d| d.join(theme))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("seance-term-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn missing_file_is_the_default_look() {
        assert_eq!(
            load(Path::new("/nonexistent/terminal.conf")),
            TermConfig::default()
        );
    }

    #[test]
    fn reads_colors_font_bell_and_notify() {
        let dir = test_dir("basic");
        let p = write(
            &dir,
            "terminal.conf",
            "# comment\n\
             palette = 1=#ff6188\n\
             background = 2d2a2e\n\
             foreground = \"#fcfcfa\"\n\
             font-size = 14\n\
             bell-features = attention,audio\n\
             bell-audio-path = sounds/hero.wav\n\
             bell-audio-volume = 0.8\n\
             notify-on-command-finish = unfocused\n\
             notify-on-command-finish-after = 1m30s\n",
        );
        let cfg = load(&p);
        assert_eq!(cfg.palette[1], 0xff6188);
        assert_eq!(cfg.palette[0], DEFAULT_PALETTE[0]);
        assert_eq!(cfg.background, 0x2d2a2e);
        assert_eq!(cfg.foreground, 0xfcfcfa);
        assert_eq!(cfg.font_size, Some(14.0));
        assert!(cfg.bell_audio);
        assert_eq!(cfg.bell_audio_path, Some(dir.join("sounds/hero.wav")));
        assert_eq!(cfg.bell_audio_volume, 0.8);
        assert_eq!(cfg.notify_on_command_finish, CommandFinishNotify::Unfocused);
        assert_eq!(cfg.notify_on_command_finish_after_ms, 90_000);
    }

    #[test]
    fn includes_load_after_the_file_and_explicit_colors_beat_the_theme() {
        let dir = test_dir("include");
        let theme = write(
            &dir,
            "mytheme",
            "background = 111111\nforeground = 222222\n",
        );
        write(
            &dir,
            "ghostty",
            "font-size = 9\nfont-size = big\nforeground = 333333\nfont-family = Main\nfont-family = Fallback\n",
        );
        let p = write(
            &dir,
            "terminal.conf",
            &format!(
                "config-file = ?\"ghostty\"\nconfig-file = ?missing\nfont-size = 12\ntheme = light:nope,dark:{}\n",
                theme.display()
            ),
        );
        let cfg = load(&p);
        // The include comes after the file's own lines, so its font-size wins.
        assert_eq!(cfg.font_size, Some(9.0));
        assert_eq!(cfg.font_family.as_deref(), Some("Main"));
        assert_eq!(cfg.background, 0x111111);
        assert_eq!(cfg.foreground, 0x333333);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("5s"), Some(5_000));
        assert_eq!(parse_duration_ms("250ms"), Some(250));
        assert_eq!(parse_duration_ms("five"), None);
    }
}
