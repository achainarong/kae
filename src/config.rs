//! User configuration. Loading is read-only; explicit theme/layout choices save.
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
    pub panes: Panes,
    #[serde(flatten)]
    extra: toml::Table,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "catppuccin-mocha".into(),
            transparent: true,
            panes: Panes::default(),
            extra: toml::Table::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Panes {
    pub files_width: u16,
    pub history_width: u16,
    pub commit_width: u16,
    pub files_height: u16,
    pub history_height: u16,
    pub commit_height: u16,
}

impl Default for Panes {
    fn default() -> Self {
        Self {
            files_width: 32,
            history_width: 50,
            commit_width: 32,
            files_height: 32,
            history_height: 50,
            commit_height: 32,
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
        Self::save(root, |document| document["theme"] = toml_edit::value(name))
    }

    pub fn save_panes(root: &Path, panes: &Panes) -> Result<()> {
        Self::save(root, |document| {
            for (name, value) in [
                ("files_width", panes.files_width),
                ("history_width", panes.history_width),
                ("commit_width", panes.commit_width),
                ("files_height", panes.files_height),
                ("history_height", panes.history_height),
                ("commit_height", panes.commit_height),
            ] {
                document["panes"][name] = toml_edit::value(i64::from(value.clamp(15, 85)));
            }
        })
    }

    fn save(root: &Path, update: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
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
        update(&mut document);
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
    fn pane_sizes_round_trip_preserve_comments_and_reject_invalid_configs() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("config.toml"),
            "# keep comment\ntheme = 'nord'\n[other]\nvalue = 42\n[panes]\ncustom = 'keep'\n",
        )
        .unwrap();
        let panes = Panes {
            files_width: 44,
            history_width: 60,
            commit_width: 0,
            history_height: 1000,
            ..Panes::default()
        };
        Config::save_panes(root.path(), &panes).unwrap();
        let config = Config::load(root.path()).unwrap();
        assert_eq!(config.panes.files_width, 44);
        assert_eq!(config.panes.history_width, 60);
        assert_eq!(config.panes.commit_width, 15);
        assert_eq!(config.panes.history_height, 85);
        Config::save_theme(root.path(), "terminal").unwrap();
        assert_eq!(Config::load(root.path()).unwrap().panes.files_width, 44);
        let text = fs::read_to_string(root.path().join("config.toml")).unwrap();
        assert!(
            text.contains("# keep comment")
                && text.contains("42")
                && text.contains("custom = 'keep'")
        );
        fs::write(
            root.path().join("config.toml"),
            "[panes]\nfiles_width = 'invalid'\n",
        )
        .unwrap();
        assert!(Config::save_panes(root.path(), &panes).is_err());
        assert!(
            fs::read_to_string(root.path().join("config.toml"))
                .unwrap()
                .contains("'invalid'")
        );
    }
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
