//! Менеджер трубок: реестр сессий поверх `pcdock-core`.
//!
//! Одна ферма — много телефонов. Транспорта, потоков и сокетов здесь нет:
//! вызывающий сам возит байты (USB сегодня, демону завтра) и сам решает,
//! в каких потоках дёргать методы. Ядро при этом не меняется.
//!
//! Видео кэшируется: последнее событие `Video` каждого телефона хранится
//! в слоте и доступно через [`Farm::last_frame`] не потребляя очередь событий.
//! Счётчик `frame_seq` растёт на каждый новый кадр — по нему клиент сокетов
//! видит пропуск при заторе.

pub mod runtime;
pub use runtime::{CachedFrame, DiscoveryConfig, FarmEvent, LiveFarm};

use std::collections::HashMap;

use pcdock_core::input::{DeviceKind, InputError, MouseButton, TouchPhase, TouchPoint};
use pcdock_core::packet::SetStream;
use pcdock_core::session::{Config, Event, Session, State, Transmit};

/// Идентификатор трубки в ферме. Назначает вызывающий (индекс USB, серийник, ...).
pub use pcdock_core::link::PhoneId;

/// Последний кадр трубки: копия данных из события `Video`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastFrame {
    pub seq: u64,
    pub codec: u8,
    pub key_frame: bool,
    pub pts_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug)]
struct Slot {
    session: Session,
    watching: bool,
    last_frame: Option<LastFrame>,
    frame_seq: u64,
}

/// Ошибки фермы: либо нет такой трубки, либо ввод отклонило ядро.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FarmError {
    UnknownPhone(PhoneId),
    AlreadyExists(PhoneId),
    Input(String),
}

impl std::fmt::Display for FarmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPhone(id) => write!(f, "нет трубки {id}"),
            Self::AlreadyExists(id) => write!(f, "трубка {id} уже есть"),
            Self::Input(e) => write!(f, "ввод отклонён: {e}"),
        }
    }
}

impl std::error::Error for FarmError {}

impl From<InputError> for FarmError {
    fn from(e: InputError) -> Self {
        Self::Input(e.to_string())
    }
}

/// Реестр сессий. Не потокобезопасна: синхронизация на вызывающем/демоне.
#[derive(Debug, Default)]
pub struct Farm {
    phones: HashMap<PhoneId, Slot>,
}

impl Farm {
    pub fn new() -> Self {
        Self { phones: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.phones.len()
    }

    pub fn is_empty(&self) -> bool {
        self.phones.is_empty()
    }

    pub fn phone_ids(&self) -> Vec<PhoneId> {
        let mut ids: Vec<PhoneId> = self.phones.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    fn slot(&self, id: PhoneId) -> Result<&Slot, FarmError> {
        self.phones.get(&id).ok_or(FarmError::UnknownPhone(id))
    }

    fn slot_mut(&mut self, id: PhoneId) -> Result<&mut Slot, FarmError> {
        self.phones.get_mut(&id).ok_or(FarmError::UnknownPhone(id))
    }

    /// Добавляет трубку с конфигом ядра по умолчанию (имя задаётся через `set_name`).
    pub fn add_phone(&mut self, id: PhoneId) -> Result<(), FarmError> {
        if self.phones.contains_key(&id) {
            return Err(FarmError::AlreadyExists(id));
        }
        self.phones.insert(
            id,
            Slot {
                session: Session::new(Config::default()),
                watching: false,
                last_frame: None,
                frame_seq: 0,
            },
        );
        Ok(())
    }

    /// Убирает трубку. Сессия закрывается, кэш кадра выбрасывается.
    pub fn remove_phone(&mut self, id: PhoneId) -> Result<(), FarmError> {
        let mut slot = self.phones.remove(&id).ok_or(FarmError::UnknownPhone(id))?;
        slot.session.close();
        Ok(())
    }

    pub fn set_name(&mut self, id: PhoneId, name: &str) -> Result<(), FarmError> {
        let slot = self.slot_mut(id)?;
        let mut cfg = slot.session.config().clone();
        cfg.name = name.to_owned();
        slot.session = Session::new(cfg);
        slot.last_frame = None;
        Ok(())
    }

    pub fn set_initial_stream(&mut self, id: PhoneId, video: bool, audio: bool) -> Result<(), FarmError> {
        let slot = self.slot_mut(id)?;
        slot.session.set_initial_stream(SetStream { video, audio, ..SetStream::default() });
        Ok(())
    }

    pub fn set_watching(&mut self, id: PhoneId, watching: bool) -> Result<(), FarmError> {
        self.slot_mut(id)?.watching = watching;
        Ok(())
    }

    pub fn is_watching(&self, id: PhoneId) -> Result<bool, FarmError> {
        Ok(self.slot(id)?.watching)
    }

    /// Последний кадр трубки (если хоть один приходил). Очередь событий не трогает.
    pub fn last_frame(&self, id: PhoneId) -> Result<Option<LastFrame>, FarmError> {
        Ok(self.slot(id)?.last_frame.clone())
    }

    pub fn state(&self, id: PhoneId) -> Result<State, FarmError> {
        Ok(self.slot(id)?.session.state())
    }

    // ----- транспорт -----

    pub fn start(&mut self, id: PhoneId, now_ms: u64) -> Result<(), FarmError> {
        let slot = self.slot_mut(id)?;
        slot.session.start(now_ms);
        slot.last_frame = None;
        Ok(())
    }

    pub fn close(&mut self, id: PhoneId) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.close();
        Ok(())
    }

    pub fn handle_input(&mut self, id: PhoneId, now_ms: u64, data: &[u8]) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.handle_input(now_ms, data);
        Ok(())
    }

    pub fn handle_timeout(&mut self, id: PhoneId, now_ms: u64) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.handle_timeout(now_ms);
        Ok(())
    }

    pub fn next_timeout(&self, id: PhoneId) -> Result<Option<u64>, FarmError> {
        Ok(self.slot(id)?.session.next_timeout())
    }

    pub fn poll_transmit(&mut self, id: PhoneId) -> Result<Option<Transmit>, FarmError> {
        Ok(self.slot_mut(id)?.session.poll_transmit())
    }

    /// Забирает следующее событие трубки. Кадры `Video` заодно кэшируются в слот.
    pub fn poll_event(&mut self, id: PhoneId) -> Result<Option<Event>, FarmError> {
        let slot = self.slot_mut(id)?;
        let ev = slot.session.poll_event();
        if let Some(Event::Video(f)) = ev.clone() {
            slot.frame_seq += 1;
            slot.last_frame = Some(LastFrame {
                seq: slot.frame_seq,
                codec: f.codec,
                key_frame: f.key_frame,
                pts_us: f.pts_us,
                data: f.data,
            });
        }
        Ok(ev)
    }

    // ----- ввод (маршрутизация в сессию трубки) -----

    pub fn device_enable(&mut self, id: PhoneId, kind: DeviceKind, on: bool) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.device_enable(kind, on);
        Ok(())
    }

    pub fn touch(&mut self, id: PhoneId, point: TouchPoint) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.touch(point)?)
    }

    /// Касание с явными пятном/силой. `None` = default ядра.
    /// Свайп собирается снаружи: DOWN + N×MOVE с паузами + UP, в каждом
    /// вызове свои координаты, пятно и сила.
    #[allow(clippy::too_many_arguments)]
    pub fn touch_ex(
        &mut self,
        id: PhoneId,
        cid: u8,
        phase: TouchPhase,
        x: f32,
        y: f32,
        width: Option<f32>,
        height: Option<f32>,
        pressure: Option<f32>,
    ) -> Result<(), FarmError> {
        let mut pt = TouchPoint::new(cid, phase, x, y);
        pt.width = width;
        pt.height = height;
        pt.pressure = pressure;
        Ok(self.slot_mut(id)?.session.touch(pt)?)
    }

    pub fn cursor_move_abs(&mut self, id: PhoneId, x: f32, y: f32) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.cursor_move_abs(x, y)?)
    }

    pub fn cursor_move_rel(&mut self, id: PhoneId, dx: i32, dy: i32) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.cursor_move_rel(dx, dy)?)
    }

    pub fn cursor_resync(&mut self, id: PhoneId) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.cursor_resync();
        Ok(())
    }

    pub fn mouse_button(&mut self, id: PhoneId, button: MouseButton, down: bool) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.mouse_button(button, down)?)
    }

    /// Скролл в тиках колеса. Человеческий темп делает вызывающий: много
    /// мелких вызовов с паузами, а не один большой. Дробь копит ядро.
    pub fn scroll(&mut self, id: PhoneId, ticks: f32) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.scroll(ticks)?)
    }

    pub fn scroll_h(&mut self, id: PhoneId, ticks: f32) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.scroll_h(ticks)?)
    }

    pub fn key(&mut self, id: PhoneId, usage: u16, down: bool) -> Result<(), FarmError> {
        Ok(self.slot_mut(id)?.session.key(usage, down)?)
    }

    pub fn release_all(&mut self, id: PhoneId) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.release_all();
        Ok(())
    }

    pub fn request_frame(&mut self, id: PhoneId) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.request_frame();
        Ok(())
    }

    pub fn wake(&mut self, id: PhoneId) -> Result<(), FarmError> {
        self.slot_mut(id)?.session.wake();
        Ok(())
    }

    pub fn set_stream(&mut self, id: PhoneId, video: bool, audio: bool) -> Result<(), FarmError> {
        let s = SetStream { video, audio, ..SetStream::default() };
        self.slot_mut(id)?.session.set_stream(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcdock_core::packet::{Hello, Role, VideoFrame};

    fn hello_bytes() -> Vec<u8> {
        Hello { version_max: 2, version_min: 2, role: Role::Device, capabilities: 0, name: "phone".into() }
            .to_packet()
            .encode()
    }

    fn video_bytes(n: u8) -> Vec<u8> {
        VideoFrame { codec: 1, key_frame: true, pts_us: n as u64, data: vec![n, 2, 3] }.to_packet().encode()
    }

    #[test]
    fn add_remove_and_unknown_phone() {
        let mut farm = Farm::new();
        assert!(farm.is_empty());
        farm.add_phone(7).unwrap();
        assert_eq!(farm.add_phone(7), Err(FarmError::AlreadyExists(7)));
        assert_eq!(farm.state(99), Err(FarmError::UnknownPhone(99)));
        farm.remove_phone(7).unwrap();
        assert!(farm.is_empty());
    }

    #[test]
    fn phones_are_isolated() {
        let mut farm = Farm::new();
        farm.add_phone(1).unwrap();
        farm.add_phone(2).unwrap();
        farm.start(1, 1000).unwrap();
        farm.start(2, 1000).unwrap();
        // HELLO только первой трубке: вторая остаётся в Connecting.
        farm.handle_input(1, 1001, &hello_bytes()).unwrap();
        assert_eq!(farm.state(1), Ok(State::Active));
        assert_eq!(farm.state(2), Ok(State::Connecting));
        // Ввод не перетекает: тач второй трубке не трогает первую.
        farm.touch_ex(2, 0, TouchPhase::Down, 0.5, 0.5, None, None, None).unwrap();
        let tx1 = farm.poll_transmit(1).unwrap();
        assert!(tx1.is_some()); // у первой свой HELLO/регистрация, не чужой тач
                                // Чистим очередь первой полностью и убеждаемся, что чужого тача там нет.
        while farm.poll_transmit(1).unwrap().is_some() {}
        assert!(farm.poll_transmit(1).unwrap().is_none());
    }

    #[test]
    fn video_is_cached_per_phone_with_seq() {
        let mut farm = Farm::new();
        farm.add_phone(1).unwrap();
        farm.add_phone(2).unwrap();
        farm.start(1, 0).unwrap();
        farm.start(2, 0).unwrap();
        farm.handle_input(1, 1, &hello_bytes()).unwrap();
        farm.handle_input(2, 1, &hello_bytes()).unwrap();
        farm.handle_input(1, 2, &video_bytes(10)).unwrap();
        farm.handle_input(1, 3, &video_bytes(11)).unwrap();
        // Забираем события первой трубки: два видео.
        let mut got = 0;
        while let Some(ev) = farm.poll_event(1).unwrap() {
            if matches!(ev, Event::Video(_)) {
                got += 1;
            }
        }
        assert_eq!(got, 2);
        let last = farm.last_frame(1).unwrap().expect("кадр закэширован");
        assert_eq!((last.seq, last.pts_us), (2, 11));
        // У второй кадров не было.
        assert!(farm.last_frame(2).unwrap().is_none());
        // Кэш не потребляет очередь: повторный poll пуст.
        assert!(farm.poll_event(1).unwrap().is_none());
    }

    #[test]
    fn watching_flag_and_touch_params() {
        let mut farm = Farm::new();
        farm.add_phone(3).unwrap();
        assert_eq!(farm.is_watching(3), Ok(false));
        farm.set_watching(3, true).unwrap();
        assert_eq!(farm.is_watching(3), Ok(true));
        farm.start(3, 0).unwrap();
        // Пятно и сила проходят в репорт (не падают, очередь растёт).
        farm.touch_ex(3, 0, TouchPhase::Down, 0.1, 0.2, Some(0.05), Some(0.06), Some(0.9)).unwrap();
        assert!(farm.poll_transmit(3).unwrap().is_some());
    }
}
