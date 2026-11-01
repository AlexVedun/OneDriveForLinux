use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Ukrainian,
}

impl Language {
    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Ukrainian => "uk",
        }
    }

    pub fn from_code(value: &str) -> Self {
        match value.trim() {
            "uk" => Self::Ukrainian,
            _ => Self::English,
        }
    }
}

fn preference_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("onedrive-linux").join("language"))
}

static LANGUAGE: OnceLock<Language> = OnceLock::new();

pub fn current() -> Language {
    *LANGUAGE.get_or_init(|| {
        preference_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .map(|value| Language::from_code(&value))
            .unwrap_or(Language::English)
    })
}

pub fn text(english: &'static str, ukrainian: &'static str) -> &'static str {
    match current() {
        Language::English => english,
        Language::Ukrainian => ukrainian,
    }
}

pub fn save(language: Language) -> std::io::Result<()> {
    let path = preference_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "configuration directory not found",
        )
    })?;
    fs::create_dir_all(path.parent().expect("language preference has a parent"))?;
    fs::write(path, format!("{}\n", language.code()))
}

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn english_is_default_and_ukrainian_is_explicit() {
        assert_eq!(Language::from_code("uk\n"), Language::Ukrainian);
        assert_eq!(Language::from_code("en"), Language::English);
        assert_eq!(Language::from_code("ru"), Language::English);
    }
}
