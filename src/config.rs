//! User configuration. Loading is read-only; only an accepted theme choice saves.
use crate::theme::{PRESETS, Theme};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub transparent: bool,
    #[serde(flatten)]
    extra: toml::Table,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "catppuccin-mocha".into(),
            transparent: true,
            extra: toml::Table::new(),
        }
    }
}

pub fn directory() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("kae"))
}

impl Config {
    pub fn initialize(root: &Path) -> Result<()> {
        fs::create_dir_all(root.join("themes"))?;
        let files = [
            ("config.toml", include_str!("../assets/config.toml")),
            (
                "themes/catppuccin-mocha.toml",
                include_str!("../assets/themes/catppuccin-mocha.toml"),
            ),
            (
                "themes/catppuccin-macchiato.toml",
                include_str!("../assets/themes/catppuccin-macchiato.toml"),
            ),
            (
                "themes/catppuccin-latte.toml",
                include_str!("../assets/themes/catppuccin-latte.toml"),
            ),
            (
                "themes/nord.toml",
                include_str!("../assets/themes/nord.toml"),
            ),
            (
                "themes/terminal.toml",
                include_str!("../assets/themes/terminal.toml"),
            ),
        ];
        for (relative, text) in files {
            let path = root.join(relative);
            if fs::symlink_metadata(&path).is_ok() {
                continue;
            }
            let mut temp =
                tempfile::NamedTempFile::new_in(path.parent().context("Missing config parent")?)?;
            temp.write_all(text.as_bytes())?;
            match temp.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error.into()),
            }
        }
        Ok(())
    }
    pub fn load(root: &Path) -> Result<Self> {
        match fs::read_to_string(root.join("config.toml")) {
            Ok(text) => Ok(toml::from_str(&text).context("Invalid kae/config.toml")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn resolve(&self, root: &Path, name: &str) -> Result<Theme> {
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "Theme names may only contain letters, numbers, - and _"
        );
        let path = root.join("themes").join(format!("{name}.toml"));
        let mut theme = match fs::read_to_string(&path) {
            Ok(text) => {
                Theme::parse(&text).with_context(|| format!("Invalid theme {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Theme::preset(name)?,
            Err(error) => return Err(error.into()),
        };
        if self.transparent {
            theme.background = ratatui::style::Color::Reset;
        }
        Ok(theme)
    }
    pub fn themes(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = PRESETS.iter().map(|name| (*name).into()).collect();
        if let Ok(entries) = fs::read_dir(root.join("themes")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "toml")
                    && let Some(name) = path.file_stem().and_then(|name| name.to_str())
                {
                    names.push(name.into());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
    pub fn save_theme(root: &Path, name: &str) -> Result<()> {
        // Re-read before saving, preserving unrelated settings and comments.
        let path = root.join("config.toml");
        let mut document: toml_edit::DocumentMut = match fs::read_to_string(&path) {
            Ok(text) => {
                let _: Config =
                    toml::from_str(&text).context("Config is invalid; not overwriting it")?;
                text.parse()?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                include_str!("../assets/config.toml").parse()?
            }
            Err(error) => return Err(error.into()),
        };
        document["theme"] = toml_edit::value(name);
        fs::create_dir_all(root)?;
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        if let Ok(metadata) = fs::metadata(&path) {
            temp.as_file().set_permissions(metadata.permissions())?;
        }
        temp.write_all(document.to_string().as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_background_is_the_default_and_opaque_is_explicit() {
        use ratatui::style::Color;
        let root = tempfile::tempdir().unwrap();
        assert!(Config::load(root.path()).unwrap().transparent);
        Config::initialize(root.path()).unwrap();
        let config = Config::load(root.path()).unwrap();
        assert!(config.transparent);
        for name in PRESETS {
            assert_eq!(
                config.resolve(root.path(), name).unwrap().background,
                Color::Reset
            );
        }
        fs::write(root.path().join("config.toml"), "theme = 'nord'\n").unwrap();
        assert!(Config::load(root.path()).unwrap().transparent);
        fs::write(
            root.path().join("config.toml"),
            "theme = 'nord'\ntransparent = false\n",
        )
        .unwrap();
        Config::initialize(root.path()).unwrap();
        Config::save_theme(root.path(), "catppuccin-mocha").unwrap();
        let config = Config::load(root.path()).unwrap();
        assert!(!config.transparent);
        assert_eq!(
            config
                .resolve(root.path(), &config.theme)
                .unwrap()
                .background,
            Theme::default().background
        );
    }
    #[test]
    fn save_preserves_settings_and_invalid_configs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("config.toml"),
            "theme = 'nord'\ntransparent = true\n[other]\nvalue = 42\n",
        )
        .unwrap();
        Config::save_theme(root.path(), "terminal").unwrap();
        let text = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(text.contains("42"));
        assert!(Config::load(root.path()).unwrap().transparent);
        fs::write(root.path().join("config.toml"), "bad = [").unwrap();
        assert!(Config::save_theme(root.path(), "nord").is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("config.toml")).unwrap(),
            "bad = ["
        );
    }
    #[test]
    fn custom_themes_load_and_path_traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("themes")).unwrap();
        fs::write(
            root.path().join("themes/custom.toml"),
            "base = 'nord'\ntext = '#123456'",
        )
        .unwrap();
        let config = Config::default();
        assert_eq!(
            config.resolve(root.path(), "custom").unwrap().text,
            ratatui::style::Color::Rgb(18, 52, 86)
        );
        assert!(config.resolve(root.path(), "../custom").is_err());
        assert!(Config::themes(root.path()).contains(&"custom".into()));
    }
    #[test]
    fn initialization_never_overwrites_user_files_and_all_templates_resolve() {
        let root = tempfile::tempdir().unwrap();
        Config::initialize(root.path()).unwrap();
        let config = Config::load(root.path()).unwrap();
        for name in PRESETS {
            config.resolve(root.path(), name).unwrap();
        }
        fs::write(
            root.path().join("themes/nord.toml"),
            "# user theme\nbase = 'nord'",
        )
        .unwrap();
        fs::write(
            root.path().join("config.toml"),
            "# keep me\ntheme = 'nord'\ntransparent = true",
        )
        .unwrap();
        Config::initialize(root.path()).unwrap();
        Config::save_theme(root.path(), "terminal").unwrap();
        assert!(
            fs::read_to_string(root.path().join("config.toml"))
                .unwrap()
                .contains("# keep me")
        );
        assert!(Config::load(root.path()).unwrap().transparent);
        assert_eq!(
            fs::read_to_string(root.path().join("themes/nord.toml")).unwrap(),
            "# user theme\nbase = 'nord'"
        );
    }
}
