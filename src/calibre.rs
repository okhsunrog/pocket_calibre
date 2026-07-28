use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config::{ensure_dir, sanitize_filename, Config};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Book {
    pub id: i64,
    pub title: String,
    pub author: String,
    /// Формат, который будем качать — первый из `formats` конфига, что есть у книги.
    pub format: Option<String>,
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    auth: Option<String>,
    formats: Vec<String>,
    limit: usize,
    /// Заполняется после первого `list()`: сервер сам сообщает id библиотеки,
    /// а он нужен для ссылок на скачивание.
    library: Option<String>,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        // Таймауты пофазные, а не глобальный: книги бывают по десятку мегабайт,
        // и по Wi-Fi ридера такая закачка легко переваливает за минуту. Общий
        // лимит рубил бы её на середине, поэтому ограничиваем только те фазы,
        // где долгое ожидание однозначно означает проблему, а само тело даём
        // качать сколько нужно — лишь бы данные шли.
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_send_request(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .user_agent("pocket_calibre/0.1")
            .build()
            .new_agent();

        let auth = cfg.user.as_ref().map(|user| {
            let pass = cfg.password.clone().unwrap_or_default();
            format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
        });

        Self {
            agent,
            base: cfg.server.trim_end_matches('/').to_string(),
            auth,
            formats: cfg.formats.clone(),
            limit: cfg.limit,
            library: cfg.library.clone(),
        }
    }

    fn get(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>, Error> {
        let mut req = self.agent.get(url);
        if let Some(auth) = &self.auth {
            req = req.header("Authorization", auth);
        }
        Ok(req.call()?)
    }

    /// Последние добавленные книги: сначала список id, потом метаданные пачкой.
    pub fn list(&mut self) -> Result<Vec<Book>, Error> {
        let search_url = match &self.library {
            Some(lib) => format!(
                "{}/ajax/search/{}?num={}&sort=timestamp&sort_order=desc",
                self.base,
                urlencode(lib),
                self.limit
            ),
            None => format!(
                "{}/ajax/search?num={}&sort=timestamp&sort_order=desc",
                self.base, self.limit
            ),
        };

        let search: Value = self.get(&search_url)?.body_mut().read_json()?;

        if let Some(lib) = search.get("library_id").and_then(Value::as_str) {
            self.library = Some(lib.to_string());
        }

        let ids: Vec<i64> = search
            .get("book_ids")
            .and_then(Value::as_array)
            .ok_or("no book_ids in the /ajax/search response")?
            .iter()
            .filter_map(Value::as_i64)
            .collect();

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut books = Vec::with_capacity(ids.len());
        // Длинный URL с сотнями id некоторые прокси режут, поэтому идём пачками.
        for chunk in ids.chunks(50) {
            books.extend(self.metadata(chunk)?);
        }

        // Порядок из /ajax/search (по дате добавления) важнее, чем порядок ответа.
        let mut by_id: std::collections::HashMap<i64, Book> =
            books.into_iter().map(|b| (b.id, b)).collect();

        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    fn metadata(&self, ids: &[i64]) -> Result<Vec<Book>, Error> {
        let joined = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");

        let url = match &self.library {
            Some(lib) => format!(
                "{}/ajax/books/{}?ids={}",
                self.base,
                urlencode(lib),
                joined
            ),
            None => format!("{}/ajax/books?ids={}", self.base, joined),
        };

        let map: Value = self.get(&url)?.body_mut().read_json()?;
        let map = map.as_object().ok_or("the /ajax/books response is not an object")?;

        let mut books = Vec::with_capacity(map.len());
        for (key, meta) in map {
            if meta.is_null() {
                continue;
            }
            let Ok(id) = key.parse::<i64>() else { continue };

            // Missing title/author stay empty here: the list substitutes a
            // localized placeholder at display time (see `Pager::render`),
            // while `download` uses fixed English constants for filenames.
            let title = meta
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            let author = meta
                .get("authors")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();

            let available: Vec<String> = meta
                .get("formats")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.to_uppercase())
                        .collect()
                })
                .unwrap_or_default();

            let format = self
                .formats
                .iter()
                .find(|want| available.iter().any(|have| have == *want))
                .cloned();

            books.push(Book {
                id,
                title,
                author,
                format,
            });
        }

        Ok(books)
    }

    /// Миниатюра обложки в JPEG. Размер задаёт сервер — просить у него готовый
    /// размер дешевле, чем тянуть полную обложку (те же 10 КБ против 230 КБ)
    /// и масштабировать на устройстве.
    pub fn thumbnail(&self, id: i64, width: u32, height: u32) -> Result<Vec<u8>, Error> {
        let library = self
            .library
            .as_ref()
            .ok_or("library id unknown, refresh the list")?;

        let url = format!(
            "{}/get/thumb/{}/{}?sz={}x{}",
            self.base,
            id,
            urlencode(library),
            width,
            height
        );

        Ok(self.get(&url)?.body_mut().read_to_vec()?)
    }

    /// Качает книгу во временный файл рядом с целевым и переименовывает его,
    /// чтобы оборванная загрузка не оставила битый файл в библиотеке ридера.
    pub fn download(&self, book: &Book, dir: &Path) -> Result<PathBuf, Error> {
        let format = book
            .format
            .as_ref()
            .ok_or("no suitable format for this book")?;
        let library = self
            .library
            .as_ref()
            .ok_or("library id unknown, refresh the list")?;

        ensure_dir(dir)?;

        // Filenames use fixed English fallbacks, not the localized ones: what
        // lands on disk must not depend on the UI language of the moment.
        let author = if book.author.is_empty() { "Unknown author" } else { &book.author };
        let title = if book.title.is_empty() { "Untitled" } else { &book.title };
        let name = sanitize_filename(&format!("{author} - {title}"));
        let target = dir.join(format!("{name}.{}", format.to_lowercase()));
        if target.exists() {
            return Ok(target);
        }

        let url = format!(
            "{}/get/{}/{}/{}",
            self.base,
            format,
            book.id,
            urlencode(library)
        );

        let tmp = dir.join(format!(".{name}.part"));
        let fetch = || -> Result<(), Error> {
            let mut resp = self.get(&url)?;
            let mut file = std::fs::File::create(&tmp)?;
            std::io::copy(&mut resp.body_mut().as_reader(), &mut file)?;
            file.flush()?;
            Ok(())
        };

        // Оборванная закачка не должна оставлять недописанный `.part` в папке
        // книг: следующая попытка его перезапишет, но если её не будет, мусор
        // так и останется лежать у пользователя на виду.
        if let Err(e) = fetch() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }

        std::fs::rename(&tmp, &target)?;
        Ok(target)
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);

        out.push(TABLE[(n >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn urlencode_escapes_non_alphanumerics() {
        assert_eq!(urlencode("Calibre_Library"), "Calibre_Library");
        assert_eq!(urlencode("my lib"), "my%20lib");
        assert_eq!(urlencode("книги"), "%D0%BA%D0%BD%D0%B8%D0%B3%D0%B8");
    }
}
