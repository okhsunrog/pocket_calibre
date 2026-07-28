//! Нативная экранная клавиатура inkview.
//!
//! `OpenKeyboardEx` — модальный UI прошивки, и звать его можно только с потока,
//! на котором крутится `InkViewMain`. UI-поток со Slint для этого не годится,
//! а «выполнить замыкание на потоке inkview» напрямую нельзя: он спит внутри
//! C-цикла и просыпается только когда C дёргает наш обработчик.
//!
//! Мост даёт `SetWeakTimerEx`: он ставит одноразовый колбэк, который inkview
//! выполнит на своём потоке. Отсюда трёхшаговая схема:
//!
//! ```text
//! UI-поток      request()  → PENDING + SetWeakTimerEx
//! поток inkview trampoline → PENDING.take() → OpenKeyboardEx
//! поток inkview on_done    → результат в канал → UI-поток забирает таймером
//! ```
//!
//! `OpenKeyboardEx` возвращается сразу, а в буфер пишет потом, поэтому буфер
//! и заголовок обязаны пережить вызов — они лежат в [`ACTIVE`] до колбэка.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use inkview::bindings::{
    Inkview, KBD_ENTEXT, KBD_NUMERIC, KBD_PASSWORD, KBD_UPPER, KBD_URL,
};

/// С запасом на длинные пути и списки форматов.
const BUFFER_LEN: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Server,
    User,
    Password,
    Library,
    DownloadDir,
    Formats,
    Limit,
}

impl Field {
    /// Индексы приходят из Slint: у него нет удобного способа передать
    /// Rust-перечисление в колбэк.
    pub fn from_index(index: i32) -> Option<Self> {
        Some(match index {
            0 => Self::Server,
            1 => Self::User,
            2 => Self::Password,
            3 => Self::Library,
            4 => Self::DownloadDir,
            5 => Self::Formats,
            6 => Self::Limit,
            _ => return None,
        })
    }

    fn flags(self) -> c_int {
        let flags = match self {
            Self::Server => KBD_URL,
            Self::Password => KBD_PASSWORD,
            Self::User | Self::Library | Self::DownloadDir => KBD_ENTEXT,
            Self::Formats => KBD_ENTEXT | KBD_UPPER,
            Self::Limit => KBD_NUMERIC,
        };
        flags as c_int
    }
}

/// Что ввёл пользователь. `None` — клавиатуру закрыли без подтверждения.
pub type Answer = (Field, Option<String>);

struct Request {
    field: Field,
    /// Заголовок диалога. Приходит из Slint уже переведённым — это подпись
    /// той самой строки настроек, по которой тапнули, второго списка этих
    /// строк на стороне Rust нет.
    title: String,
    initial: String,
}

/// Заявка от UI-потока, которую заберёт поток inkview.
static PENDING: Mutex<Option<Request>> = Mutex::new(None);

/// Пока клавиатура открыта, inkview держит указатели внутрь этой структуры.
/// Трогает её только поток inkview, поэтому гонок за буфер нет.
struct Active {
    field: Field,
    title: CString,
    buffer: Box<[c_char; BUFFER_LEN]>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
static IV: OnceLock<&'static Inkview> = OnceLock::new();
static ANSWERS: OnceLock<Sender<Answer>> = OnceLock::new();

pub fn init(iv: &'static Inkview, answers: Sender<Answer>) {
    let _ = IV.set(iv);
    let _ = ANSWERS.set(answers);
}

/// Просит открыть клавиатуру. Вызывается с UI-потока и возвращается сразу:
/// ответ придёт в канал, переданный в [`init`].
pub fn request(field: Field, title: String, initial: String) {
    let Some(iv) = IV.get() else { return };

    // Вторая заявка, пока не отработала первая, затирала бы её: trampoline
    // забирает PENDING ровно один раз, и первое поле молча терялось бы. Про
    // «уже открыта» спрашиваем прошивку, а не свой ACTIVE: если колбэк
    // почему-то не придёт, мы не запрём клавиатуру навсегда.
    let mut pending = PENDING.lock().unwrap();
    if pending.is_some() || is_open(iv) {
        return;
    }
    *pending = Some(Request { field, title, initial });
    drop(pending);

    unsafe {
        iv.SetWeakTimerEx(
            c"pocket_calibre_kbd".as_ptr(),
            Some(trampoline),
            std::ptr::null_mut(),
            1,
        );
    }
}

/// Открыта ли клавиатура прямо сейчас. Пока открыта, ввод принадлежит ей и в
/// Slint его слать не надо.
pub fn is_open(iv: &Inkview) -> bool {
    unsafe { iv.IsKeyboardOpened() != 0 }
}

/// Выполняется на потоке inkview.
unsafe extern "C" fn trampoline(_context: *mut c_void) {
    let (Some(iv), Some(request)) = (IV.get(), PENDING.lock().unwrap().take()) else {
        return;
    };

    let Ok(title) = CString::new(request.title) else {
        return;
    };

    let mut buffer = Box::new([0 as c_char; BUFFER_LEN]);
    let bytes = request.initial.as_bytes();
    // Обрезаем по границе буфера; хвост уже нулевой, терминатор на месте.
    let len = bytes.len().min(BUFFER_LEN - 1);
    for (slot, byte) in buffer.iter_mut().zip(&bytes[..len]) {
        *slot = *byte as c_char;
    }

    let flags = request.field.flags();

    // Указатели берём уже после того, как структура легла в статик: буфер и
    // заголовок должны оставаться живыми до колбэка, а inkview вернётся из
    // OpenKeyboardEx немедленно.
    let mut guard = ACTIVE.lock().unwrap();
    *guard = Some(Active {
        field: request.field,
        title,
        buffer,
    });
    let active = guard.as_mut().unwrap();
    let title_ptr = active.title.as_ptr();
    let buffer_ptr = active.buffer.as_mut_ptr();
    drop(guard);

    unsafe {
        iv.OpenKeyboardEx(
            title_ptr,
            buffer_ptr,
            (BUFFER_LEN - 1) as c_int,
            flags,
            Some(on_done),
            std::ptr::null_mut(),
        );
    }
}

/// Выполняется на потоке inkview после закрытия клавиатуры.
unsafe extern "C" fn on_done(text: *mut c_char, _context: *mut c_void) {
    // Читаем ДО того, как отпустим ACTIVE: `text` указывает в тот самый буфер.
    let value = if text.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned())
    };

    let Some(active) = ACTIVE.lock().unwrap().take() else {
        return;
    };

    if let Some(answers) = ANSWERS.get() {
        let _ = answers.send((active.field, value));
    }
}
