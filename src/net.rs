use inkview::bindings::{Inkview, NET_CONNECTED};

/// Connecting to the network failed; carries the `NetConnect` result code.
/// A typed error rather than a message: the UI thread renders it in the
/// current language (see [`crate::i18n::Status::NetworkFailed`]).
#[derive(Debug, Clone, Copy)]
pub struct ConnectError(pub i32);

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "couldn't connect to the network (code {})", self.0)
    }
}

impl std::error::Error for ConnectError {}

/// Поднимает Wi-Fi, если он выключен.
///
/// `QueryNetwork` возвращает битовую маску состояния. `NET_CONNECTED` — это не
/// один бит, а маска 0xF00 поверх бит готовности интерфейсов (`NET_BTREADY`,
/// `NET_WIFIREADY`, `NET_CDMA3GREADY`), поэтому берём константу из SDK, а не
/// пишем число руками: младшие биты маски состояния — это `NET_BLUETOOTH`,
/// `NET_WIFI`, `NET_CDMA3G`, то есть «интерфейс вообще есть», а не «подключён».
///
/// `NetConnect(NULL)` подключается к последней использованной сети и сам
/// показывает системный диалог, если нужна ручная настройка.
pub fn ensure_online(iv: &Inkview) -> Result<(), ConnectError> {
    unsafe {
        if iv.QueryNetwork() & NET_CONNECTED as i32 != 0 {
            return Ok(());
        }

        let result = iv.NetConnect(std::ptr::null());
        if result == 0 {
            Ok(())
        } else {
            Err(ConnectError(result))
        }
    }
}
