mod calibre;
mod config;
mod i18n;
mod keyboard;
mod libm_shim;
mod net;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use inkview::Event;
use inkview::bindings::Inkview;
use inkview::event::Key;
use inkview::screen::Screen;
use slint::{ComponentHandle, Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, TimerMode, VecModel};

use calibre::{Book, Client};
use config::{Config, LoadNote};
use i18n::{Lang, LangChoice, Status};
use keyboard::Field;

slint::include_modules!();

/// Thumbnail size we ask the server for, in physical pixels.
///
/// This is the size the cover is actually drawn at: a list row is 11 mm tall
/// and the image is 0.66 of that wide, which on the PB632's 300 dpi panel comes
/// out at roughly 86x130 px. Asking for more only cost memory and bandwidth,
/// since Slint scaled it straight back down. Tied to 300 dpi — a denser panel
/// would want this derived from `Screen::dpi` instead.
const COVER_SIZE: (u32, u32) = (88, 132);

/// How many covers we keep in memory.
///
/// At 4 bits per pixel a cover is about 5.8 KB, so the cache tops out near
/// 370 KB — some eight pages of history. The bound matters because `limit`
/// allows 5000 books: unbounded, paging through a library grew without end on a
/// device with 512 MB of RAM. It also makes eviction safe to do blindly, as 64
/// is far more than a single page can hold.
const COVER_CACHE_LIMIT: usize = 64;

/// Команды от UI к рабочему потоку.
enum Cmd {
    Refresh,
    Download(i32),
    Reconfigure(Config),
    Covers(Vec<i64>),
}

/// Ответы рабочего потока к UI. Тексты здесь не ходят: статусы и состояния
/// книг передаются данными, а во фразы на текущем языке их превращает
/// UI-поток (см. `i18n`) — иначе смена языка не перерисовала бы их.
enum Msg {
    Status(Status),
    Busy(bool),
    Books(Vec<Book>),
    BookState(i32, i18n::BookState),
    Cover { id: i64, cover: CachedCover },
}

/// A cached cover: 4 bits per pixel, two pixels to a byte.
///
/// The panel shows 16 grey levels, so a nibble per pixel is everything that
/// survives being displayed — colour and the low bits of luminance are dropped
/// by the screen regardless. Against decoded RGB that is a 32x saving (5.8 KB
/// versus 190 KB for the same picture), and it is what keeps [`COVER_CACHE_LIMIT`]
/// covers well under a megabyte.
///
/// Rows are byte-aligned. The server preserves aspect ratio when fitting a cover
/// into [`COVER_SIZE`], so the width is whatever it likes — odd values included.
struct CachedCover {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl CachedCover {
    fn stride(width: u32) -> usize {
        (width as usize).div_ceil(2)
    }

    /// Decodes a JPEG thumbnail and packs it. Called on the worker thread: this
    /// is the expensive half, and the UI thread must not stall on it.
    fn pack(jpeg: &[u8]) -> Result<Self, calibre::Error> {
        let gray = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.to_luma8();

        Ok(Self::from_gray(&gray))
    }

    fn from_gray(gray: &image::GrayImage) -> Self {
        let (width, height) = gray.dimensions();
        let stride = Self::stride(width);

        let mut pixels = vec![0u8; stride * height as usize];
        for (x, y, px) in gray.enumerate_pixels() {
            // 0..=255 -> 0..=15: the top nibble is the grey level.
            let level = px.0[0] >> 4;
            let byte = &mut pixels[y as usize * stride + x as usize / 2];
            if x.is_multiple_of(2) {
                *byte = level << 4;
            } else {
                *byte |= level;
            }
        }

        Self {
            width,
            height,
            pixels,
        }
    }

    /// Grey level of a pixel, 0..=15.
    fn level(&self, x: usize, y: usize) -> u8 {
        let byte = self.pixels[y * Self::stride(self.width) + x / 2];

        if x.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 0x0f
        }
    }

    /// Expands back to RGB, the only thing Slint takes — it has no grayscale
    /// `Image` constructor. Cheap enough to redo on every render: a byte loop,
    /// not a decode, which is the whole reason the cache holds pixels rather
    /// than the server's JPEG.
    fn to_image(&self) -> Image {
        let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(self.width, self.height);
        let width = self.width as usize;
        let out = buffer.make_mut_slice();

        for y in 0..self.height as usize {
            for x in 0..width {
                // Nibble back to a whole byte: duplicating it sends 15 to 255,
                // where a bare shift would stop at 240 and grey out the whites.
                let level = self.level(x, y);
                let value = level << 4 | level;
                out[y * width + x] = Rgb8Pixel {
                    r: value,
                    g: value,
                    b: value,
                };
            }
        }

        Image::from_rgb8(buffer)
    }
}

fn main() {
    // Inkview живёт всё время работы процесса, а Screen и рабочий поток хотят
    // ссылку с 'static — отсюда утечка вместо возни с Arc.
    let iv: &'static Inkview = Box::leak(Box::new(inkview::load()));
    let (evt_tx, evt_rx) = mpsc::channel();
    let (redraw_tx, redraw_rx) = mpsc::channel();
    let (lang_tx, lang_rx) = mpsc::channel();

    std::thread::Builder::new()
        .name("ui".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || ui_main(iv, evt_rx, redraw_rx, lang_rx))
        .expect("не удалось запустить UI-поток");

    inkview::iv_main(iv, move |event| {
        // EVT_EXIT обрабатываем первым и до всех фильтров ниже: наш обработчик
        // отвечает прошивке RES_EVENT_HANDLED, то есть «событие принято», и
        // закрыть приложение обязаны мы сами. Бэкенд inkview-slint это событие
        // не переводит вовсе, а его цикл — бесконечный `loop`, так что без
        // явного CloseApp выйти из приложения штатно нельзя.
        if matches!(event, Event::Exit) {
            unsafe {
                iv.CloseApp();
            }
            return Some(());
        }

        // Язык прошивки читается только здесь, на потоке inkview, — как и все
        // прочие вызовы с ним связанные (см. `keyboard`). При старте значение
        // уходит в канал раньше EVT_INIT, так что к моменту, когда UI-поток
        // начнёт строить окно, язык уже известен. Смена языка в системных
        // настройках приходит клавишей LanguageChange — перечитываем и просим
        // полную перерисовку: прошивка рисовала поверх нас свои меню.
        if matches!(event, Event::Init)
            || matches!(event, Event::KeyDown { key: Key::LanguageChange })
        {
            let _ = lang_tx.send(i18n::system_lang(iv));
        }
        if matches!(event, Event::KeyDown { key: Key::LanguageChange }) {
            let _ = redraw_tx.send(());
        }

        // Пока открыта нативная клавиатура, ввод принадлежит ей: пробрасывать
        // тапы в Slint значило бы жать кнопки под клавиатурой.
        if keyboard::is_open(iv) {
            return Some(());
        }

        // PointerMove не пробрасываем. Палец генерирует их десятками в секунду,
        // а цикл бэкенда обрабатывает одно событие за кадр, где кадр — это
        // полная отрисовка плюс обновление e-ink. Очередь росла быстрее, чем
        // разгребалась, и интерфейс уползал за пальцем. Для тапа эти события не
        // нужны: press и release несут свои координаты.
        if matches!(event, Event::PointerMove { .. }) {
            return Some(());
        }

        // Бэкенд inkview-slint эти события игнорирует, а между тем именно они
        // означают «кадр на экране больше не наш»: возврат из фона, закрытие
        // системного диалога. Отводим их в отдельный канал, чтобы UI-поток
        // перерисовался целиком.
        if matches!(event, Event::Repaint | Event::Show) {
            let _ = redraw_tx.send(());
        }

        // Обрыв канала означает, что UI-поток умер. Продолжать бессмысленно:
        // события уходили бы в никуда, а приложение висело бы с мёртвым
        // экраном, поэтому закрываемся штатно.
        if evt_tx.send(event).is_err() {
            unsafe {
                iv.CloseApp();
            }
        }

        Some(())
    });
}

/// Список книг постранично.
///
/// Прокрутки нет намеренно: на e-ink каждый кадр стоит полного обновления
/// экрана, поэтому список нарезается здесь, а Slint показывает ровно одну
/// готовую страницу. Сколько строк в неё влезает, считает сам Slint — от
/// фактической высоты окна, — и отдаёт через `rows-per-page`.
struct Pager {
    window: slint::Weak<MainWindow>,
    model: Rc<VecModel<BookItem>>,
    cmd_tx: Sender<Cmd>,

    all: RefCell<Vec<Book>>,
    covers: RefCell<HashMap<i64, CachedCover>>,
    /// Порядок попадания в `covers` — по нему вытесняем самые старые обложки.
    cover_order: RefCell<VecDeque<i64>>,
    /// Что уже заказано у рабочего потока. Без этого каждая пришедшая обложка
    /// снова просила бы все ещё не пришедшие: на страницу из N книг выходило
    /// порядка N²/2 запросов к серверу.
    requested: RefCell<HashSet<i64>>,
    states: RefCell<HashMap<i64, i18n::BookState>>,
    /// Текущий язык интерфейса — общая ячейка с `ui_main`. Нужен прямо в
    /// `render()`: состояния книг, подписи-заглушки и номер страницы
    /// переводятся при каждой отрисовке.
    lang: Rc<Cell<Lang>>,
    page: Cell<usize>,
    rows: Cell<usize>,
    /// Модель изменилась, но перерисовку отложили до конца разбора очереди
    /// сообщений — см. [`Pager::flush`].
    dirty: Cell<bool>,
}

impl Pager {
    fn new(
        window: slint::Weak<MainWindow>,
        model: Rc<VecModel<BookItem>>,
        cmd_tx: Sender<Cmd>,
        lang: Rc<Cell<Lang>>,
    ) -> Self {
        Self {
            window,
            model,
            cmd_tx,
            all: RefCell::new(Vec::new()),
            covers: RefCell::new(HashMap::new()),
            cover_order: RefCell::new(VecDeque::new()),
            requested: RefCell::new(HashSet::new()),
            states: RefCell::new(HashMap::new()),
            lang,
            page: Cell::new(0),
            rows: Cell::new(1),
            dirty: Cell::new(false),
        }
    }

    fn set_books(&self, books: Vec<Book>) {
        *self.all.borrow_mut() = books;
        self.covers.borrow_mut().clear();
        self.cover_order.borrow_mut().clear();
        self.requested.borrow_mut().clear();
        self.states.borrow_mut().clear();
        self.page.set(0);
        self.render();
    }

    /// Число строк на странице меняется вместе с размером окна, поэтому
    /// приходит не один раз при старте.
    fn set_rows(&self, rows: usize) {
        if self.rows.replace(rows.max(1)) != rows.max(1) {
            self.render();
        }
    }

    fn turn(&self, forward: bool) {
        let page = self.page.get();
        let next = if forward {
            page + 1
        } else {
            page.saturating_sub(1)
        };

        if next != page && next < self.total_pages() {
            self.page.set(next);
            self.render();
        }
    }

    fn set_state(&self, id: i64, state: i18n::BookState) {
        self.states.borrow_mut().insert(id, state);
        self.dirty.set(true);
    }

    fn set_cover(&self, id: i64, cover: CachedCover) {
        {
            let mut covers = self.covers.borrow_mut();
            let mut order = self.cover_order.borrow_mut();

            if covers.insert(id, cover).is_none() {
                order.push_back(id);
            }

            // Вытесняем самые старые. Заодно забываем, что их заказывали:
            // если пользователь вернётся на ту страницу, обложка загрузится
            // заново, а не останется дырой.
            while order.len() > COVER_CACHE_LIMIT {
                let Some(old) = order.pop_front() else { break };
                covers.remove(&old);
                self.requested.borrow_mut().remove(&old);
            }
        }

        // Обложка книги с другой страницы ничего на экране не меняет, а
        // перерисовка на e-ink стоит полного обновления области. Пока
        // пользователь листает вперёд, ответы на заказы предыдущих страниц
        // продолжают приходить — молча кладём их в кэш.
        if self.is_visible(id) {
            self.dirty.set(true);
        }
    }

    /// Перерисовывает, если с прошлого раза что-то менялось. Зовётся один раз
    /// на разбор очереди сообщений: иначе пачка из N обложек давала бы N
    /// полных сбросов модели подряд.
    fn flush(&self) {
        if self.dirty.get() {
            self.render();
        }
    }

    fn total_pages(&self) -> usize {
        self.all.borrow().len().div_ceil(self.rows.get().max(1)).max(1)
    }

    /// Диапазон видимых книг в `all` для текущей страницы.
    fn page_range(&self) -> (usize, usize) {
        let len = self.all.borrow().len();
        let rows = self.rows.get().max(1);
        let page = self.page.get().min(len.div_ceil(rows).max(1) - 1);

        let start = (page * rows).min(len);
        (start, (start + rows).min(len))
    }

    fn is_visible(&self, id: i64) -> bool {
        let (start, end) = self.page_range();

        self.all.borrow()[start..end].iter().any(|b| b.id == id)
    }

    fn render(&self) {
        self.dirty.set(false);

        let Some(window) = self.window.upgrade() else {
            return;
        };

        let all = self.all.borrow();
        let rows = self.rows.get().max(1);
        let total = all.len().div_ceil(rows).max(1);

        // Страница могла оказаться за концом списка: сменились настройки,
        // повернули экран, пришёл более короткий список.
        let page = self.page.get().min(total - 1);
        self.page.set(page);

        let start = (page * rows).min(all.len());
        let end = (start + rows).min(all.len());
        let visible = &all[start..end];

        let covers = self.covers.borrow();
        let states = self.states.borrow();
        let lang = self.lang.get();

        self.model.set_vec(
            visible
                .iter()
                .map(|book| BookItem {
                    id: book.id as i32,
                    // Сервер мог не прислать название или автора — тогда поле
                    // пустое (см. `calibre::Client::metadata`), и заглушку на
                    // текущем языке подставляем здесь, при показе.
                    title: if book.title.is_empty() {
                        i18n::untitled(lang).into()
                    } else {
                        book.title.clone().into()
                    },
                    author: if book.author.is_empty() {
                        i18n::unknown_author(lang).into()
                    } else {
                        book.author.clone().into()
                    },
                    format: book.format.clone().unwrap_or_else(|| "—".to_string()).into(),
                    state: states
                        .get(&book.id)
                        .map(|s| i18n::book_state(*s, lang))
                        .unwrap_or_default()
                        .into(),
                    cover: covers.get(&book.id).map(CachedCover::to_image).unwrap_or_default(),
                })
                .collect::<Vec<_>>(),
        );

        window.set_page_label(if all.is_empty() {
            Default::default()
        } else {
            i18n::page_label(page + 1, total, lang).into()
        });
        window.set_has_prev(page > 0);
        window.set_has_next(page + 1 < total);

        // Обложки грузим только для показанной страницы — это и есть главная
        // выгода пагинации перед списком на 200 строк. Уже заказанные
        // пропускаем: render() зовётся и на каждую пришедшую обложку, так что
        // без этой проверки страница переспрашивала бы сама себя по кругу.
        let mut requested = self.requested.borrow_mut();
        let missing: Vec<i64> = visible
            .iter()
            .map(|book| book.id)
            .filter(|id| !covers.contains_key(id) && requested.insert(*id))
            .collect();

        if !missing.is_empty() {
            let _ = self.cmd_tx.send(Cmd::Covers(missing));
        }
    }
}

fn ui_main(
    iv: &'static Inkview,
    evt_rx: Receiver<Event>,
    redraw_rx: Receiver<()>,
    lang_rx: Receiver<Lang>,
) {
    // До EVT_INIT трогать экран нельзя.
    loop {
        match evt_rx.recv() {
            Ok(Event::Init) => break,
            Ok(_) => continue,
            Err(_) => return,
        }
    }

    let screen = Screen::new(iv);
    // Физический dpi экрана — основа вёрстки: все размеры задаются в
    // миллиметрах и переводятся в логические пиксели с учётом dpi и
    // scale_factor. Для PB632 dpi=300; на всякий случай подстраховываемся от
    // нулевого значения, чтобы не делить интерфейс на ноль.
    let screen_dpi = screen.dpi().max(1) as f32;
    let backend = inkview_slint::Backend::new(screen, evt_rx);
    slint::platform::set_platform(Box::new(backend)).expect("платформа уже установлена");

    // Язык прошивки поток inkview кладёт в канал до EVT_INIT, так что здесь
    // он уже дожидается. Дальше эти две ячейки живут на UI-потоке: системный
    // язык и действующий (с учётом настройки в конфиге).
    let sys_lang = Rc::new(Cell::new(lang_rx.try_recv().unwrap_or_default()));

    let (loaded, cfg_path, note) = Config::load();
    let choice = LangChoice::from_config(loaded.language.as_deref());
    let lang = Rc::new(Cell::new(choice.resolve(sys_lang.get())));

    let window = MainWindow::new().expect("не удалось создать окно");

    // Только после создания первого компонента: до него у Slint ещё нет
    // глобального контекста, в котором живёт выбор перевода.
    if let Err(e) = slint::select_bundled_translation(lang.get().code()) {
        eprintln!("select_bundled_translation({}): {e}", lang.get().code());
    }

    // Настройки живут на UI-потоке: их показывает экран настроек, правит
    // клавиатура и сохраняет в файл. Рабочий поток получает копию.
    let cfg = Rc::new(RefCell::new(loaded.clone()));

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (answer_tx, answer_rx) = mpsc::channel::<keyboard::Answer>();

    keyboard::init(iv, answer_tx);

    // Идентификатор сборки виден на устройстве постоянно — чтобы не гадать,
    // какой билд запущен.
    window.set_build_id(concat!("build ", env!("BUILD_ID")).into());

    let books = Rc::new(VecModel::<BookItem>::default());
    window.set_books(ModelRc::from(books.clone()));

    // Последний статус храним данными, а не строкой: при смене языка его
    // нужно перерисовать заново — см. `apply_language`.
    let last_status: Rc<RefCell<Option<Status>>> = Rc::new(RefCell::new(None));

    let initial = match note {
        Some(LoadNote::ParseError { path, error }) => Status::ConfigError {
            path: path.display().to_string(),
            error,
        },
        Some(LoadNote::Created) => Status::ConfigMissing,
        Some(LoadNote::CreateFailed { path, error }) => Status::ConfigCreateFailed {
            path: path.display().to_string(),
            error,
        },
        None => Status::ConfigPath(cfg_path.display().to_string()),
    };
    set_status(&window, &last_status, lang.get(), initial);
    show_config(&window, &cfg.borrow());
    window.set_cfg_language(i18n::language_row_value(choice, sys_lang.get()).into());

    let pager = Rc::new(Pager::new(
        window.as_weak(),
        books,
        cmd_tx.clone(),
        lang.clone(),
    ));

    std::thread::Builder::new()
        .name("worker".to_string())
        .spawn(move || worker(iv, loaded, cmd_rx, msg_tx))
        .expect("не удалось запустить рабочий поток");

    window.on_refresh({
        let cmd_tx = cmd_tx.clone();
        move || {
            let _ = cmd_tx.send(Cmd::Refresh);
        }
    });

    window.on_download({
        let cmd_tx = cmd_tx.clone();
        move |id| {
            let _ = cmd_tx.send(Cmd::Download(id));
        }
    });

    window.on_prev_page({
        let pager = pager.clone();
        move || pager.turn(false)
    });

    window.on_next_page({
        let pager = pager.clone();
        move || pager.turn(true)
    });

    window.on_toggle_settings({
        let weak = window.as_weak();
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.set_settings_open(!window.get_settings_open());
        }
    });

    window.on_edit_field({
        let cfg = cfg.clone();
        move |index, title| {
            let Some(field) = Field::from_index(index) else {
                return;
            };
            // Пароль всегда набирается заново: показывать его нечем и
            // подставлять в клавиатуру незачем.
            let initial = match field {
                Field::Password => String::new(),
                other => current_value(&cfg.borrow(), other),
            };
            // Заголовок клавиатуры — подпись строки настроек, уже на текущем
            // языке: Slint передал её из @tr-литерала.
            keyboard::request(field, title.into(), initial);
        }
    });

    window.on_cycle_language({
        let cfg = cfg.clone();
        let cfg_path = cfg_path.clone();
        let weak = window.as_weak();
        let pager = pager.clone();
        let last_status = last_status.clone();
        let lang = lang.clone();
        let sys_lang = sys_lang.clone();
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };

            let choice = {
                let mut cfg = cfg.borrow_mut();
                let next = LangChoice::from_config(cfg.language.as_deref()).next();
                cfg.language = next.to_config();
                next
            };

            // Смена языка нарочно НЕ шлёт Cmd::Reconfigure: клиенту calibre
            // язык безразличен, а Reconfigure сбросил бы список книг.
            if let Err(e) = cfg.borrow().save(&cfg_path) {
                set_status(
                    &window,
                    &last_status,
                    lang.get(),
                    Status::SettingsSaveFailed(e.to_string()),
                );
            }

            apply_language(&window, &pager, &last_status, &lang, choice, sys_lang.get());
        }
    });

    // Бэкенд inkview-slint не реализует event loop proxy, поэтому
    // `invoke_from_event_loop` из чужого потока не сработает. Забираем
    // результаты таймером: его крутит штатный механизм Slint внутри цикла
    // бэкенда.
    let timer = slint::Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let weak = window.as_weak();
        let pager = pager.clone();
        let cfg = cfg.clone();
        let cmd_tx = cmd_tx.clone();
        let cfg_path = cfg_path.clone();
        let last_status = last_status.clone();
        let lang = lang.clone();
        let sys_lang = sys_lang.clone();

        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };

            // Пользователь сменил язык в системных настройках, пока мы
            // работали. В режиме «Авто» интерфейс следует за ним; при явном
            // выборе обновится только подпись «Авто (…)» в строке настроек.
            while let Ok(system) = lang_rx.try_recv() {
                sys_lang.set(system);
                let choice = LangChoice::from_config(cfg.borrow().language.as_deref());
                apply_language(&window, &pager, &last_status, &lang, choice, system);
            }

            // scale_factor бэкенд выставляет только после старта своего цикла,
            // поэтому «миллиметр в логических пикселях» пересчитываем здесь.
            // 1 мм = dpi/25.4 физических px, а логические = физические /
            // scale_factor. Отсюда вся вёрстка получает физически корректный
            // масштаб на любом экране.
            let scale = window.window().scale_factor();
            if scale > 0.0 {
                let mm = screen_dpi / 25.4 / scale;
                let metrics = window.global::<Metrics>();
                if (metrics.get_mm() - mm).abs() > 0.001 {
                    metrics.set_mm(mm);
                }
            }

            pager.set_rows(window.get_rows_per_page().max(1) as usize);

            while let Ok(msg) = msg_rx.try_recv() {
                apply(&window, &pager, &last_status, lang.get(), msg);
            }
            // Одна перерисовка на всю разобранную пачку: пришедшие разом
            // обложки и статусы книг иначе дали бы по полному сбросу модели
            // каждая, а на e-ink это заметно.
            pager.flush();

            while let Ok((field, value)) = answer_rx.try_recv() {
                if let Some(s) = apply_answer(&window, &cfg, &cfg_path, &cmd_tx, field, value) {
                    set_status(&window, &last_status, lang.get(), s);
                }
            }

            if redraw_rx.try_iter().count() > 0 {
                window.set_repaint_tick(window.get_repaint_tick() + 1);
            }
        }
    });

    let _ = cmd_tx.send(Cmd::Refresh);

    window.run().expect("цикл событий завершился с ошибкой");
}

fn apply(
    window: &MainWindow,
    pager: &Rc<Pager>,
    last_status: &RefCell<Option<Status>>,
    lang: Lang,
    msg: Msg,
) {
    match msg {
        Msg::Status(s) => set_status(window, last_status, lang, s),
        Msg::Busy(busy) => window.set_busy(busy),
        Msg::Books(list) => pager.set_books(list),
        Msg::BookState(id, state) => pager.set_state(id as i64, state),
        Msg::Cover { id, cover } => pager.set_cover(id, cover),
    }
}

/// Показывает статус на текущем языке и запоминает его данными: при смене
/// языка `apply_language` перерисует его заново.
fn set_status(window: &MainWindow, last: &RefCell<Option<Status>>, lang: Lang, s: Status) {
    window.set_status(i18n::status(&s, lang).into());
    *last.borrow_mut() = Some(s);
}

/// Переключает язык интерфейса. Общий путь для тапа по строке «Язык» и для
/// смены языка в системных настройках (в режиме «Авто»).
fn apply_language(
    window: &MainWindow,
    pager: &Rc<Pager>,
    last_status: &RefCell<Option<Status>>,
    lang_cell: &Cell<Lang>,
    choice: LangChoice,
    system: Lang,
) {
    // Подпись строки обновляется всегда: «Авто (English)» → «English» — это
    // видимое изменение, даже когда действующий язык остался прежним.
    window.set_cfg_language(i18n::language_row_value(choice, system).into());

    let new = choice.resolve(system);
    if lang_cell.replace(new) == new {
        return;
    }

    // Все @tr-строки Slint перерисовывает сам; вручную перерисовываем то,
    // что рендерит Rust: список (состояния, заглушки, номер страницы) и
    // последний статус. Плюс полный кадр — e-ink иначе оставит артефакты.
    if let Err(e) = slint::select_bundled_translation(new.code()) {
        eprintln!("select_bundled_translation({}): {e}", new.code());
    }
    pager.render();
    if let Some(s) = &*last_status.borrow() {
        window.set_status(i18n::status(s, new).into());
    }
    window.set_repaint_tick(window.get_repaint_tick() + 1);
}

/// Показывает текущие настройки на экране настроек.
fn show_config(window: &MainWindow, cfg: &Config) {
    let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".to_string());

    window.set_server(cfg.server.clone().into());
    window.set_cfg_server(cfg.server.clone().into());
    window.set_cfg_user(or_dash(&cfg.user).into());
    window.set_cfg_password(if cfg.password.is_some() { "••••••" } else { "—" }.into());
    window.set_cfg_library(or_dash(&cfg.library).into());
    window.set_cfg_download_dir(cfg.download_dir.display().to_string().into());
    window.set_cfg_formats(cfg.formats.join(", ").into());
    window.set_cfg_limit(cfg.limit.to_string().into());
}

fn current_value(cfg: &Config, field: Field) -> String {
    match field {
        Field::Server => cfg.server.clone(),
        Field::User => cfg.user.clone().unwrap_or_default(),
        Field::Password => cfg.password.clone().unwrap_or_default(),
        Field::Library => cfg.library.clone().unwrap_or_default(),
        Field::DownloadDir => cfg.download_dir.display().to_string(),
        Field::Formats => cfg.formats.join(","),
        Field::Limit => cfg.limit.to_string(),
    }
}

/// Принимает то, что набрали на клавиатуре: обновляет настройки, пишет их на
/// диск и пересобирает клиента в рабочем потоке. Возвращает статус для строки
/// состояния — показывает его вызывающий, у которого есть текущий язык.
fn apply_answer(
    window: &MainWindow,
    cfg: &Rc<RefCell<Config>>,
    cfg_path: &Path,
    cmd_tx: &Sender<Cmd>,
    field: Field,
    value: Option<String>,
) -> Option<Status> {
    // Клавиатуру всегда закрывают поверх нашего кадра, даже если ввод отменили,
    // поэтому перерисовываем в любом случае.
    window.set_repaint_tick(window.get_repaint_tick() + 1);

    let value = value?;
    let value = value.trim().to_string();
    let optional = |v: String| if v.is_empty() { None } else { Some(v) };

    let before = cfg.borrow().clone();

    {
        let mut cfg = cfg.borrow_mut();
        match field {
            Field::Server => cfg.server = value.trim_end_matches('/').to_string(),
            Field::User => cfg.user = optional(value),
            Field::Password => cfg.password = optional(value),
            Field::Library => cfg.library = optional(value),
            Field::DownloadDir if !value.is_empty() => cfg.download_dir = PathBuf::from(value),
            Field::Formats if !value.is_empty() => {
                cfg.formats = value
                    .split(',')
                    .map(|f| f.trim().to_uppercase())
                    .filter(|f| !f.is_empty())
                    .collect();
            }
            Field::Limit => {
                if let Ok(n) = value.parse::<usize>() {
                    cfg.limit = n.clamp(1, 5000);
                }
            }
            // Пустое значение для пути или списка форматов — не изменение,
            // а очистка обязательного поля; молча оставляем прежнее.
            Field::DownloadDir | Field::Formats => {}
        }
    }

    let cfg = cfg.borrow();
    show_config(window, &cfg);

    // Reconfigure пересоздаёт клиента и сбрасывает список книг, поэтому шлём
    // его только если настройки правда изменились. Иначе достаточно было
    // открыть поле и нажать «ОК», ничего не правя, — и список пропадал.
    if *cfg == before {
        return None;
    }

    let status = match cfg.save(cfg_path) {
        Ok(()) => Status::SettingsSaved,
        Err(e) => Status::SettingsSaveFailed(e.to_string()),
    };

    let _ = cmd_tx.send(Cmd::Reconfigure(cfg.clone()));
    Some(status)
}

fn worker(iv: &'static Inkview, cfg: Config, cmd_rx: Receiver<Cmd>, msg_tx: Sender<Msg>) {
    let mut cfg = cfg;
    let mut client = Client::new(&cfg);
    let mut known: Vec<Book> = Vec::new();

    while let Ok(cmd) = cmd_rx.recv() {
        // Обложки грузятся фоном и строку состояния не занимают: пользователь
        // в это время листает список, и «Подождите…» было бы враньём.
        let noisy = !matches!(cmd, Cmd::Covers(_));
        if noisy {
            let _ = msg_tx.send(Msg::Busy(true));
        }

        match cmd {
            // Клиент кэширует адрес, авторизацию и id библиотеки, поэтому при
            // смене настроек его проще пересоздать, чем править по частям.
            Cmd::Reconfigure(updated) => {
                cfg = updated;
                client = Client::new(&cfg);
                known.clear();
                let _ = msg_tx.send(Msg::Books(Vec::new()));
            }

            Cmd::Covers(ids) => {
                for id in ids {
                    let Ok(jpeg) = client.thumbnail(id, COVER_SIZE.0, COVER_SIZE.1) else {
                        continue;
                    };
                    // A book with no cover is routine — leave the space blank
                    // rather than complaining in the status line.
                    if let Ok(cover) = CachedCover::pack(&jpeg) {
                        let _ = msg_tx.send(Msg::Cover { id, cover });
                    }
                }
            }

            Cmd::Refresh => {
                let _ = msg_tx.send(Msg::Status(Status::CheckingNetwork));

                match net::ensure_online(iv) {
                    Ok(()) => {
                        let _ = msg_tx.send(Msg::Status(Status::LoadingList));
                        match client.list() {
                            Ok(list) => {
                                let _ = msg_tx.send(Msg::Status(Status::BookCount(list.len())));
                                known = list.clone();
                                let _ = msg_tx.send(Msg::Books(list));
                            }
                            Err(e) => {
                                let _ =
                                    msg_tx.send(Msg::Status(Status::ListFailed(e.to_string())));
                            }
                        }
                    }
                    Err(net::ConnectError(code)) => {
                        let _ = msg_tx.send(Msg::Status(Status::NetworkFailed(code)));
                    }
                }
            }

            Cmd::Download(id) => match known.iter().find(|b| b.id as i32 == id).cloned() {
                None => {
                    let _ = msg_tx.send(Msg::Status(Status::BookNotFound));
                }
                Some(book) if book.format.is_none() => {
                    let _ = msg_tx.send(Msg::BookState(id, i18n::BookState::NoFormat));
                    let _ = msg_tx.send(Msg::Status(Status::NoFormat {
                        title: book.title.clone(),
                    }));
                }
                Some(book) => {
                    let _ = msg_tx.send(Msg::BookState(id, i18n::BookState::InProgress));
                    let _ = msg_tx.send(Msg::Status(Status::Downloading {
                        title: book.title.clone(),
                    }));

                    // Сетевая ошибка и ошибка самой закачки — разные статусы,
                    // поэтому ветвление явное, а не одна цепочка and_then.
                    let outcome = match net::ensure_online(iv) {
                        Err(net::ConnectError(code)) => Err(Status::NetworkFailed(code)),
                        Ok(()) => client
                            .download(&book, &cfg.download_dir)
                            .map_err(|e| Status::DownloadFailed(e.to_string())),
                    };

                    match outcome {
                        Ok(path) => {
                            let _ = msg_tx.send(Msg::BookState(id, i18n::BookState::Done));
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let _ = msg_tx.send(Msg::Status(Status::Saved { name }));
                        }
                        Err(status) => {
                            let _ = msg_tx.send(Msg::BookState(id, i18n::BookState::Failed));
                            let _ = msg_tx.send(Msg::Status(status));
                        }
                    }
                }
            },
        }

        if noisy {
            let _ = msg_tx.send(Msg::Busy(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_packing_round_trip() {
        // Width 3 is odd on purpose: the server preserves aspect ratio, so it
        // hands back whatever width fits, and rows still have to land on a
        // byte boundary.
        let gray = image::GrayImage::from_raw(3, 2, vec![0, 15, 255, 16, 240, 128]).unwrap();
        let packed = CachedCover::from_gray(&gray);

        assert_eq!((packed.width, packed.height), (3, 2));
        // Two bytes per row rather than three for the whole image.
        assert_eq!(packed.pixels.len(), 4);

        assert_eq!(packed.level(0, 0), 0);
        assert_eq!(packed.level(1, 0), 0);
        assert_eq!(packed.level(2, 0), 15);
        assert_eq!(packed.level(0, 1), 1);
        assert_eq!(packed.level(1, 1), 15);
        assert_eq!(packed.level(2, 1), 8);

        // The padding nibble of an odd row must stay clear, or it would read
        // back as a stray dark pixel at the start of the next row.
        assert_eq!(packed.pixels[1] & 0x0f, 0);
    }

    #[test]
    fn white_survives_the_round_trip() {
        let gray = image::GrayImage::from_raw(2, 1, vec![255, 0]).unwrap();
        let packed = CachedCover::from_gray(&gray);

        // Duplicating the nibble is what keeps white at 255; a bare shift left
        // would cap it at 240 and tint every cover grey.
        let white = packed.level(0, 0);
        assert_eq!(white << 4 | white, 255);

        let black = packed.level(1, 0);
        assert_eq!(black << 4 | black, 0);
    }
}
