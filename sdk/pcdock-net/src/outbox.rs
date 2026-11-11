use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

/// Сколько событий JSON (штук и байт) может накопиться у клиента, прежде чем он будет отключён как не читающий.
const JSON_QUEUE_LIMIT: usize = 4096;
const JSON_QUEUE_BYTES: usize = 2 * 1024 * 1024;
/// Сколько пакетов «с потерями» (звук) держим для клиента; лишние отбрасываются с начала.
const LOSSY_QUEUE_LIMIT: usize = 256;
/// Сколько пакетов «надёжных данных» (файлы) может ждать отправки; производитель ждёт, пока клиент прочитает.
const BULK_QUEUE_LIMIT: usize = 8;

/// Исходящее сообщение клиенту, готовое к отправке носителем.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Json(String),
    Binary { kind: u8, payload: Vec<u8> },
}

#[derive(Default)]
struct State {
    json: VecDeque<String>,
    json_bytes: usize,
    bulk: VecDeque<(u8, Vec<u8>)>,
    latest: Option<(u8, Vec<u8>)>,
    lossy: VecDeque<(u8, Vec<u8>)>,
    closed: bool,
}

/// Исходящий ящик клиента. Порядок выдачи: события JSON, надёжные данные, последний кадр, пакеты с потерями.
///
/// - JSON не теряется; клиент, у которого он копится без чтения, отключается;
/// - «последний кадр» хранится одним слотом: незабранный кадр заменяется новым (быстрый клиент получает
///   каждый кадр, медленный всегда самый свежий);
/// - пакеты «с потерями» (звук) идут очередью ограниченной длины, лишние отбрасываются;
/// - «надёжные данные» (файлы) идут очередью с ожиданием: производитель блокируется, пока клиент не прочитает.
#[derive(Default)]
pub struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
    space: Condvar,
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Кладёт событие JSON. `false` — ящик закрыт или переполнен: клиент не читает и будет отключён.
    pub fn json(&self, text: String) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return false;
        }
        if st.json.len() >= JSON_QUEUE_LIMIT || st.json_bytes + text.len() > JSON_QUEUE_BYTES {
            st.closed = true;
            self.ready.notify_all();
            self.space.notify_all();
            return false;
        }
        st.json_bytes += text.len();
        st.json.push_back(text);
        self.ready.notify_one();
        true
    }

    /// Заменяет слот «последний кадр».
    pub fn latest(&self, kind: u8, payload: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        st.latest = Some((kind, payload));
        self.ready.notify_one();
    }

    /// Кладёт пакет в очередь с потерями.
    pub fn lossy(&self, kind: u8, payload: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return;
        }
        if st.lossy.len() >= LOSSY_QUEUE_LIMIT {
            st.lossy.pop_front();
        }
        st.lossy.push_back((kind, payload));
        self.ready.notify_one();
    }

    /// Кладёт надёжный пакет и ждёт, если очередь полна. `false` — ящик закрыт.
    pub fn bulk(&self, kind: u8, payload: Vec<u8>) -> bool {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.closed {
                return false;
            }
            if st.bulk.len() < BULK_QUEUE_LIMIT {
                break;
            }
            st = self.space.wait(st).unwrap();
        }
        st.bulk.push_back((kind, payload));
        self.ready.notify_one();
        true
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
        self.space.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    fn take(&self, st: &mut State) -> Option<Out> {
        if let Some(j) = st.json.pop_front() {
            st.json_bytes -= j.len();
            return Some(Out::Json(j));
        }
        if let Some((kind, payload)) = st.bulk.pop_front() {
            self.space.notify_one();
            return Some(Out::Binary { kind, payload });
        }
        if let Some((kind, payload)) = st.latest.take() {
            return Some(Out::Binary { kind, payload });
        }
        st.lossy.pop_front().map(|(kind, payload)| Out::Binary { kind, payload })
    }

    /// Ждёт сообщение. `None` — ящик закрыт.
    pub fn pop_blocking(&self) -> Option<Out> {
        let mut st = self.state.lock().unwrap();
        loop {
            if st.closed {
                return None;
            }
            if let Some(m) = self.take(&mut st) {
                return Some(m);
            }
            st = self.ready.wait(st).unwrap();
        }
    }

    /// Берёт сообщение, если оно есть (для писателя, который ещё и читает).
    pub fn pop_now(&self) -> Option<Out> {
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return None;
        }
        self.take(&mut st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_json_then_bulk_then_latest_then_lossy() {
        let o = Outbox::new();
        o.lossy(2, vec![2]);
        o.latest(1, vec![1]);
        assert!(o.bulk(3, vec![3]));
        assert!(o.json("{}".into()));
        let kinds: Vec<_> = std::iter::from_fn(|| o.pop_now()).collect();
        assert_eq!(
            kinds,
            vec![
                Out::Json("{}".into()),
                Out::Binary { kind: 3, payload: vec![3] },
                Out::Binary { kind: 1, payload: vec![1] },
                Out::Binary { kind: 2, payload: vec![2] },
            ]
        );
    }

    #[test]
    fn latest_slot_keeps_only_the_newest() {
        let o = Outbox::new();
        o.latest(1, vec![1]);
        o.latest(1, vec![2]);
        assert_eq!(o.pop_now(), Some(Out::Binary { kind: 1, payload: vec![2] }));
        assert_eq!(o.pop_now(), None);
    }

    #[test]
    fn json_overflow_closes_the_outbox() {
        let o = Outbox::new();
        for _ in 0..JSON_QUEUE_LIMIT {
            assert!(o.json("x".into()));
        }
        assert!(!o.json("x".into()), "переполнение отключает клиента");
        assert!(o.is_closed());
        assert_eq!(o.pop_now(), None);
    }

    #[test]
    fn bulk_blocks_until_the_reader_catches_up() {
        let o = std::sync::Arc::new(Outbox::new());
        for i in 0..BULK_QUEUE_LIMIT {
            assert!(o.bulk(3, vec![i as u8]));
        }
        let o2 = o.clone();
        let t = std::thread::spawn(move || o2.bulk(3, vec![99]));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!t.is_finished(), "производитель ждёт, пока очередь полна");
        assert!(o.pop_now().is_some());
        assert!(t.join().unwrap());
    }
}
