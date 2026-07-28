//! UI language handling: detection, selection and Rust-side strings.
//!
//! Strings living in `ui/app.slint` are translated by Slint itself: `@tr()`
//! literals plus a bundled gettext catalog (see `lang/` and
//! `slint::select_bundled_translation`). Everything the Rust side puts on
//! screen — the status line, book states, the page label — is represented as
//! data (enums with parameters) and rendered to text here, on the UI thread.
//! That is what makes live language switching possible: the worker thread
//! never bakes a user-visible sentence, it only reports what happened.

use std::ffi::CStr;

use inkview::bindings::Inkview;

/// Effective UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Ru,
}

impl Lang {
    /// Normalizes whatever the firmware reports. The exact shape of
    /// `currentLang()` is undocumented — it may be a bare code ("ru"), a
    /// locale ("ru_RU.UTF-8") or a full name ("russian") — so match on the
    /// prefix and fall back to English for anything unrecognized.
    pub fn normalize(raw: &str) -> Self {
        let lower = raw.trim().to_ascii_lowercase();
        if lower.starts_with("ru") {
            Lang::Ru
        } else {
            Lang::En
        }
    }

    /// The identifier `slint::select_bundled_translation` expects. "en" is
    /// the source language of the `@tr()` literals: Slint resolves it to the
    /// untranslated strings even though no en.po is bundled.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }
}

/// What the config asks for: follow the firmware language or force one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LangChoice {
    #[default]
    Auto,
    En,
    Ru,
}

impl LangChoice {
    /// Reads the `language` config key. Junk values mean Auto rather than an
    /// error: the file is hand-editable and a typo should not wedge the UI.
    pub fn from_config(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("en") => LangChoice::En,
            Some("ru") => LangChoice::Ru,
            _ => LangChoice::Auto,
        }
    }

    /// Auto is stored as an absent key, so a config predating the setting
    /// and one cycled back to Auto look identical.
    pub fn to_config(self) -> Option<String> {
        match self {
            LangChoice::Auto => None,
            LangChoice::En => Some("en".to_string()),
            LangChoice::Ru => Some("ru".to_string()),
        }
    }

    /// The order the settings row cycles through on tap.
    pub fn next(self) -> Self {
        match self {
            LangChoice::Auto => LangChoice::En,
            LangChoice::En => LangChoice::Ru,
            LangChoice::Ru => LangChoice::Auto,
        }
    }

    pub fn resolve(self, system: Lang) -> Lang {
        match self {
            LangChoice::Auto => system,
            LangChoice::En => Lang::En,
            LangChoice::Ru => Lang::Ru,
        }
    }
}

/// Reads the firmware UI language. Must be called on the inkview thread —
/// the same rule as every other stateful inkview call (see `keyboard`).
///
/// Goes through the raw `libloading` field rather than the generated method:
/// the method panics if the symbol is missing from the device's
/// `libinkview.so`, and a reader without `currentLang` should just get an
/// English UI, not a crash.
pub fn system_lang(iv: &Inkview) -> Lang {
    let Ok(current_lang) = iv.currentLang.as_ref() else {
        return Lang::En;
    };
    let ptr = unsafe { current_lang() };
    if ptr.is_null() {
        return Lang::En;
    }
    Lang::normalize(&unsafe { CStr::from_ptr(ptr) }.to_string_lossy())
}

/// Per-book download state, shown in the right-hand column of a list row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookState {
    NoFormat,
    InProgress,
    Done,
    Failed,
}

pub fn book_state(state: BookState, lang: Lang) -> &'static str {
    match (state, lang) {
        (BookState::InProgress, _) => "…",
        (BookState::NoFormat, Lang::En) => "no format",
        (BookState::NoFormat, Lang::Ru) => "нет формата",
        (BookState::Done, Lang::En) => "done",
        (BookState::Done, Lang::Ru) => "готово",
        (BookState::Failed, Lang::En) => "error",
        (BookState::Failed, Lang::Ru) => "ошибка",
    }
}

/// Everything the status line can say. Error details carried inside (`ureq`,
/// `io`, server anomalies) stay in English by design: they are diagnostics
/// interleaved with library error text, not sentences we author.
#[derive(Debug, Clone)]
pub enum Status {
    ConfigPath(String),
    ConfigError { path: String, error: String },
    ConfigMissing,
    ConfigCreateFailed { path: String, error: String },
    CheckingNetwork,
    NetworkFailed(i32),
    LoadingList,
    BookCount(usize),
    ListFailed(String),
    BookNotFound,
    NoFormat { title: String },
    Downloading { title: String },
    Saved { name: String },
    DownloadFailed(String),
    SettingsSaved,
    SettingsSaveFailed(String),
}

pub fn status(s: &Status, lang: Lang) -> String {
    // A book without a title reaches here as an empty string (see
    // `calibre::Client::metadata`); show the localized placeholder instead.
    let or_untitled = |title: &str| {
        if title.is_empty() {
            untitled(lang).to_string()
        } else {
            title.to_string()
        }
    };

    match (s, lang) {
        (Status::ConfigPath(path), Lang::En) => format!("Config: {path}"),
        (Status::ConfigPath(path), Lang::Ru) => format!("Конфиг: {path}"),
        (Status::ConfigError { path, error }, Lang::En) => format!("Error in {path}: {error}"),
        (Status::ConfigError { path, error }, Lang::Ru) => format!("Ошибка в {path}: {error}"),
        (Status::ConfigMissing, Lang::En) => "No settings found — set the server address".into(),
        (Status::ConfigMissing, Lang::Ru) => {
            "Настройки не найдены — укажите адрес сервера".into()
        }
        (Status::ConfigCreateFailed { path, error }, Lang::En) => {
            format!("Couldn't create {path}: {error}")
        }
        (Status::ConfigCreateFailed { path, error }, Lang::Ru) => {
            format!("Не удалось создать {path}: {error}")
        }
        (Status::CheckingNetwork, Lang::En) => "Checking network…".into(),
        (Status::CheckingNetwork, Lang::Ru) => "Проверяю сеть…".into(),
        (Status::NetworkFailed(code), Lang::En) => {
            format!("Couldn't connect to the network (code {code})")
        }
        (Status::NetworkFailed(code), Lang::Ru) => {
            format!("Не удалось подключиться к сети (код {code})")
        }
        (Status::LoadingList, Lang::En) => "Loading book list…".into(),
        (Status::LoadingList, Lang::Ru) => "Загружаю список книг…".into(),
        (Status::BookCount(n), Lang::En) => format!("Books listed: {n}"),
        (Status::BookCount(n), Lang::Ru) => format!("Книг в списке: {n}"),
        (Status::ListFailed(e), Lang::En) => format!("Couldn't fetch the list: {e}"),
        (Status::ListFailed(e), Lang::Ru) => format!("Не удалось получить список: {e}"),
        (Status::BookNotFound, Lang::En) => "Book not found, refresh the list".into(),
        (Status::BookNotFound, Lang::Ru) => "Книга не найдена, обновите список".into(),
        (Status::NoFormat { title }, Lang::En) => {
            format!("“{}”: none of the wanted formats", or_untitled(title))
        }
        (Status::NoFormat { title }, Lang::Ru) => {
            format!("«{}»: нет ни одного из нужных форматов", or_untitled(title))
        }
        (Status::Downloading { title }, Lang::En) => {
            format!("Downloading “{}”…", or_untitled(title))
        }
        (Status::Downloading { title }, Lang::Ru) => {
            format!("Скачиваю «{}»…", or_untitled(title))
        }
        (Status::Saved { name }, Lang::En) => format!("Saved: {name}"),
        (Status::Saved { name }, Lang::Ru) => format!("Сохранено: {name}"),
        (Status::DownloadFailed(e), Lang::En) => format!("Download error: {e}"),
        (Status::DownloadFailed(e), Lang::Ru) => format!("Ошибка загрузки: {e}"),
        (Status::SettingsSaved, Lang::En) => "Settings saved — press “Refresh”".into(),
        (Status::SettingsSaved, Lang::Ru) => "Настройки сохранены — нажмите «Обновить»".into(),
        (Status::SettingsSaveFailed(e), Lang::En) => format!("Couldn't save settings: {e}"),
        (Status::SettingsSaveFailed(e), Lang::Ru) => format!("Не удалось сохранить настройки: {e}"),
    }
}

/// Display fallback for a book with no title. Filenames deliberately do not
/// use this: they take an English constant in `calibre::Client::download`,
/// so what lands on disk does not depend on the UI language.
pub fn untitled(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Untitled",
        Lang::Ru => "Без названия",
    }
}

pub fn unknown_author(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "Unknown author",
        Lang::Ru => "Неизвестный автор",
    }
}

/// "p. 3 / 12" under the book list. Pages are 1-based on screen.
pub fn page_label(page: usize, total: usize, lang: Lang) -> String {
    match lang {
        Lang::En => format!("p. {page} / {total}"),
        Lang::Ru => format!("стр. {page} / {total}"),
    }
}

/// Value text of the Language settings row. Auto spells out what it
/// currently resolves to, so the row is informative before the first tap.
pub fn language_row_value(choice: LangChoice, system: Lang) -> String {
    match choice {
        LangChoice::Auto => match system {
            Lang::En => format!("Auto ({})", system.name()),
            Lang::Ru => format!("Авто ({})", system.name()),
        },
        LangChoice::En => Lang::En.name().to_string(),
        LangChoice::Ru => Lang::Ru.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handles_firmware_variants() {
        assert_eq!(Lang::normalize("ru"), Lang::Ru);
        assert_eq!(Lang::normalize("RU"), Lang::Ru);
        assert_eq!(Lang::normalize("ru_RU.UTF-8"), Lang::Ru);
        assert_eq!(Lang::normalize("russian"), Lang::Ru);
        assert_eq!(Lang::normalize(" ru "), Lang::Ru);
        assert_eq!(Lang::normalize("en"), Lang::En);
        assert_eq!(Lang::normalize("english"), Lang::En);
        assert_eq!(Lang::normalize(""), Lang::En);
        assert_eq!(Lang::normalize("uk"), Lang::En);
        assert_eq!(Lang::normalize("de_DE"), Lang::En);
    }

    #[test]
    fn choice_round_trips_through_config() {
        for choice in [LangChoice::Auto, LangChoice::En, LangChoice::Ru] {
            let stored = choice.to_config();
            assert_eq!(LangChoice::from_config(stored.as_deref()), choice);
        }

        assert_eq!(LangChoice::from_config(None), LangChoice::Auto);
        assert_eq!(LangChoice::from_config(Some("auto")), LangChoice::Auto);
        assert_eq!(LangChoice::from_config(Some(" en ")), LangChoice::En);
        // A typo must not wedge the UI in a surprise language.
        assert_eq!(LangChoice::from_config(Some("gibberish")), LangChoice::Auto);
    }

    #[test]
    fn choice_cycle_covers_all_states() {
        assert_eq!(LangChoice::Auto.next(), LangChoice::En);
        assert_eq!(LangChoice::En.next(), LangChoice::Ru);
        assert_eq!(LangChoice::Ru.next(), LangChoice::Auto);
    }

    #[test]
    fn resolve_follows_system_only_on_auto() {
        assert_eq!(LangChoice::Auto.resolve(Lang::Ru), Lang::Ru);
        assert_eq!(LangChoice::Auto.resolve(Lang::En), Lang::En);
        assert_eq!(LangChoice::En.resolve(Lang::Ru), Lang::En);
        assert_eq!(LangChoice::Ru.resolve(Lang::En), Lang::Ru);
    }

    #[test]
    fn status_renders_in_both_languages() {
        assert_eq!(
            status(&Status::BookCount(7), Lang::En),
            "Books listed: 7"
        );
        assert_eq!(
            status(&Status::BookCount(7), Lang::Ru),
            "Книг в списке: 7"
        );
        assert_eq!(
            status(&Status::Downloading { title: "War and Peace".into() }, Lang::En),
            "Downloading “War and Peace”…"
        );
        // An empty title renders as the localized placeholder.
        assert_eq!(
            status(&Status::Downloading { title: String::new() }, Lang::Ru),
            "Скачиваю «Без названия»…"
        );
    }

    #[test]
    fn language_row_value_spells_out_auto() {
        assert_eq!(
            language_row_value(LangChoice::Auto, Lang::Ru),
            "Авто (Русский)"
        );
        assert_eq!(
            language_row_value(LangChoice::Auto, Lang::En),
            "Auto (English)"
        );
        assert_eq!(language_row_value(LangChoice::En, Lang::Ru), "English");
        assert_eq!(language_row_value(LangChoice::Ru, Lang::En), "Русский");
    }
}
