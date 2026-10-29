//! Semantic colors shared by every workspace, dialog, and editor.
use anyhow::{Result, bail, ensure};
use ratatui::style::Color;
use serde::Deserialize;

pub const PRESETS: &[&str] = &[
    "catppuccin-mocha",
    "catppuccin-macchiato",
    "catppuccin-latte",
    "nord",
    "terminal",
];

#[derive(Clone, Debug)]
pub struct Theme {
    pub background: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub selection: Color,
    pub added: Color,
    pub deleted: Color,
    pub warning: Color,
    pub hunk: Color,
    pub search: Color,
    pub search_active: Color,
    pub search_text: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::preset("catppuccin-mocha").expect("built-in theme")
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    #[serde(default = "default_base")]
    pub base: String,
    pub background: Option<String>,
    pub text: Option<String>,
    pub muted: Option<String>,
    pub border: Option<String>,
    pub accent: Option<String>,
    pub selection: Option<String>,
    pub added: Option<String>,
    pub deleted: Option<String>,
    pub warning: Option<String>,
    pub hunk: Option<String>,
    pub search: Option<String>,
    pub search_active: Option<String>,
    pub search_text: Option<String>,
}
fn default_base() -> String {
    "catppuccin-mocha".into()
}

fn color(value: &str) -> Result<Color> {
    if value == "terminal" {
        return Ok(Color::Reset);
    }
    ensure!(
        value.len() == 7
            && value.starts_with('#')
            && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Expected #RRGGBB or terminal, got {value:?}"
    );
    Ok(Color::Rgb(
        u8::from_str_radix(&value[1..3], 16)?,
        u8::from_str_radix(&value[3..5], 16)?,
        u8::from_str_radix(&value[5..7], 16)?,
    ))
}

impl Theme {
    pub fn preset(name: &str) -> Result<Self> {
        let values = match name {
            "catppuccin-mocha" => [
                "#1e1e2e", "#cdd6f4", "#a6adc8", "#45475a", "#b4befe", "#45475a", "#a6e3a1",
                "#f38ba8", "#f9e2af", "#9399b2", "#f9e2af", "#cba6f7",
            ],
            "catppuccin-macchiato" => [
                "#24273a", "#cad3f5", "#a5adcb", "#494d64", "#b7bdf8", "#494d64", "#a6da95",
                "#ed8796", "#eed49f", "#939ab7", "#eed49f", "#c6a0f6",
            ],
            "catppuccin-latte" => [
                "#eff1f5", "#4c4f69", "#6c6f85", "#bcc0cc", "#7287fd", "#ccd0da", "#287d16",
                "#c22040", "#8f6400", "#6c6f85", "#dfb751", "#b99be8",
            ],
            "nord" => [
                "#2e3440", "#eceff4", "#b0bacb", "#4c566a", "#88c0d0", "#434c5e", "#a3be8c",
                "#bf616a", "#ebcb8b", "#81a1c1", "#ebcb8b", "#b48ead",
            ],
            "terminal" => {
                return Ok(Self {
                    background: Color::Reset,
                    text: Color::Reset,
                    muted: Color::Gray,
                    border: Color::DarkGray,
                    accent: Color::Blue,
                    selection: Color::DarkGray,
                    added: Color::Green,
                    deleted: Color::Red,
                    warning: Color::Yellow,
                    hunk: Color::Gray,
                    search: Color::Yellow,
                    search_active: Color::Magenta,
                    search_text: Color::Black,
                });
            }
            _ => bail!("Unknown built-in theme: {name}"),
        };
        let colors = values.map(|value| color(value).expect("built-in hex color"));
        let [
            background,
            text,
            muted,
            border,
            accent,
            selection,
            added,
            deleted,
            warning,
            hunk,
            search,
            search_active,
        ] = colors;
        Ok(Self {
            background,
            text,
            muted,
            border,
            accent,
            selection,
            added,
            deleted,
            warning,
            hunk,
            search,
            search_active,
            search_text: color("#1e1e2e")?,
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: ThemeFile = toml::from_str(text)?;
        let mut theme = Self::preset(&file.base)?;
        macro_rules! apply { ($($field:ident),*) => { $(if let Some(value) = file.$field { theme.$field = color(&value)?; })* }; }
        apply!(
            background,
            text,
            muted,
            border,
            accent,
            selection,
            added,
            deleted,
            warning,
            hunk,
            search,
            search_active,
            search_text
        );
        Ok(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_and_overrides_are_validated() {
        for name in PRESETS {
            Theme::preset(name).unwrap();
        }
        assert_eq!(
            Theme::parse("accent = '#123456'").unwrap().accent,
            Color::Rgb(18, 52, 86)
        );
        for bad in [
            "accent = 'red'",
            "base = 'missing'",
            "unexpected = '#ffffff'",
            "text = '#12雪'",
        ] {
            assert!(Theme::parse(bad).is_err());
        }
    }
}
