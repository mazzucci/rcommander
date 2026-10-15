//! Settings persistence: `<config dir>/rcommander/settings.toml`
//! (`~/.config` on Linux, `~/Library/Application Support` on macOS).
//! `RCOMMANDER_CONFIG` overrides the file path.

use anyhow::{Context, Result};
use rc_core::Settings;
use std::path::PathBuf;

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("RCOMMANDER_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rcommander")
        .join("settings.toml")
}

/// Load settings; a missing file gives the defaults. A malformed file is an error so
/// the user's settings are never silently replaced.
pub fn load_from(path: &std::path::Path) -> Result<Settings> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid settings in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn save_to(path: &std::path::Path, settings: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(settings)?;
    // Write then rename, so a crash never leaves a half-written file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

pub fn load() -> Result<Settings> {
    load_from(&path())
}

pub fn save(settings: &Settings) -> Result<()> {
    save_to(&path(), settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_core::Column;

    #[test]
    fn round_trip_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sub/settings.toml");
        assert_eq!(load_from(&p).unwrap(), Settings::default());
        let mut s = Settings::default();
        s.toggle_column(Column::Permissions);
        s.editor = "code --wait".into();
        save_to(&p, &s).unwrap();
        assert_eq!(load_from(&p).unwrap(), s);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("permissions") && text.contains("code --wait"), "{text}");
        std::fs::write(&p, "columns = 3").unwrap();
        assert!(load_from(&p).is_err());
    }
}
