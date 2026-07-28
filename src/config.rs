use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Настройки в TOML. Править их можно и с устройства — экран настроек
/// вызывает нативную клавиатуру inkview, см. [`crate::keyboard`], — и с
/// компьютера, положив файл рядом с приложением.
///
/// `serde(default)` означает, что отсутствующий ключ берётся из [`Default`]:
/// файл можно урезать до одного `server`, и всё остальное подставится.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library: Option<String>,
    pub download_dir: PathBuf,
    pub formats: Vec<String>,
    pub limit: usize,
    /// UI language: absent = follow the firmware ("auto"), or "en"/"ru".
    /// Parsed leniently by [`crate::i18n::LangChoice::from_config`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: "http://192.168.1.10:8080".to_string(),
            user: None,
            password: None,
            library: None,
            download_dir: PathBuf::from("/mnt/ext1/Books"),
            formats: vec!["FB2".into(), "EPUB".into(), "PDF".into(), "MOBI".into()],
            limit: 200,
            language: None,
        }
    }
}

/// The header is prepended on every save: `toml` does not keep comments,
/// and the file must stay readable for whoever opens it by hand.
const HEADER: &str = "\
# pocket_calibre settings. Rewritten by the in-app settings screen.
# server        — calibre content server address
# user/password — if the server requires auth (basic only)
# library       — library id; the server default is used when absent
# formats       — in order of preference, the first available one is downloaded
# language      — ui language: omit the key for auto, or set en / ru

";

/// What went wrong (or notably right) while loading the config. Kept as data
/// rather than a message so the UI thread can render it in the current
/// language — this module stays UI-agnostic.
#[derive(Debug)]
pub enum LoadNote {
    ParseError { path: PathBuf, error: String },
    Created,
    CreateFailed { path: PathBuf, error: String },
}

impl Config {
    /// Ищет конфиг рядом с исполняемым файлом, затем в стандартных папках
    /// PocketBook. Если ничего нет — создаёт файл с настройками по умолчанию
    /// и возвращает их вместе с путём.
    ///
    /// Третий элемент — заметка для строки состояния, если что-то пошло
    /// не так или конфига не было вовсе.
    pub fn load() -> (Self, PathBuf, Option<LoadNote>) {
        let candidates = Self::candidate_paths();

        for path in &candidates {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };

            return match toml::from_str::<Self>(&text) {
                Ok(cfg) => (cfg, path.clone(), None),
                // Битый файл не перезаписываем: пользователь правил его руками,
                // и молча стереть правки хуже, чем поработать на умолчаниях.
                Err(e) => (
                    Self::default(),
                    path.clone(),
                    Some(LoadNote::ParseError {
                        path: path.clone(),
                        error: e.to_string(),
                    }),
                ),
            };
        }

        let target = candidates
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from("pocket_calibre.toml"));

        let defaults = Self::default();
        let note = match defaults.save(&target) {
            Ok(()) => LoadNote::Created,
            Err(e) => LoadNote::CreateFailed {
                path: target.clone(),
                error: e.to_string(),
            },
        };

        (defaults, target, Some(note))
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let body = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        std::fs::write(path, format!("{HEADER}{body}"))
    }

    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            paths.push(dir.join("pocket_calibre.toml"));
        }
        paths.push(PathBuf::from("/mnt/ext1/applications/pocket_calibre.toml"));
        paths.push(PathBuf::from("/mnt/ext1/system/config/pocket_calibre.toml"));

        paths
    }
}

/// Убирает из имени файла всё, что может не понравиться FAT-разделу ридера.
pub fn sanitize_filename(name: &str) -> String {
    // Хвостовые точки и пробелы на FAT недопустимы, а ведущие пробелы просто
    // мусор. Точки и пробелы снимаются вперемешку, иначе «имя. .» оставило бы
    // хвост после первого же прохода.
    let trim = |s: &str| {
        s.trim_start()
            .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string()
    };

    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    let mut cleaned = trim(&cleaned);

    // Ограничение длины имени на FAT32 — 255 байт, режем с запасом по символам.
    if cleaned.chars().count() > 120 {
        // Обрезка могла оставить на конце ту самую точку или пробел, ради
        // которых был первый trim, — повторяем его уже по факту.
        cleaned = trim(&cleaned.chars().take(120).collect::<String>());
    }

    if cleaned.is_empty() {
        "book".to_string()
    } else {
        cleaned
    }
}

pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        Ok(())
    } else {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_round_trip() {
        let cfg = Config {
            server: "http://192.0.2.10:8080".to_string(),
            library: Some("Calibre_Library".to_string()),
            // Кавычки и пробелы — то, на чём ломался прежний key=value формат.
            password: Some("па \"роль\" с = и #".to_string()),
            download_dir: PathBuf::from("/mnt/ext1/Мои книги"),
            language: Some("ru".to_string()),
            ..Default::default()
        };

        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.server, cfg.server);
        assert_eq!(parsed.library, cfg.library);
        assert_eq!(parsed.password, cfg.password);
        assert_eq!(parsed.download_dir, cfg.download_dir);
        assert_eq!(parsed.formats, cfg.formats);
        assert_eq!(parsed.limit, cfg.limit);
        assert_eq!(parsed.language, cfg.language);
    }

    #[test]
    fn sanitize_filename_cleans_edges_and_length() {
        assert_eq!(sanitize_filename("Автор - Книга"), "Автор - Книга");
        assert_eq!(sanitize_filename("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize_filename("  имя. . "), "имя");
        assert_eq!(sanitize_filename(""), "book");
        assert_eq!(sanitize_filename("..."), "book");

        // Обрезка длинного имени не должна оставлять хвостовую точку.
        let long = format!("{}. хвост", "я".repeat(119));
        let cut = sanitize_filename(&long);
        assert_eq!(cut.chars().count(), 119);
        assert!(!cut.ends_with('.') && !cut.ends_with(' '));
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let parsed: Config = toml::from_str("server = \"http://host:8080\"").unwrap();

        assert_eq!(parsed.server, "http://host:8080");
        assert_eq!(parsed.user, None);
        assert_eq!(parsed.limit, Config::default().limit);
        assert_eq!(parsed.formats, Config::default().formats);
        // Configs written before the language setting existed load as auto.
        assert_eq!(parsed.language, None);
    }
}
