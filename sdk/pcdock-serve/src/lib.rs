//! Связь с демоном: один протокол, четыре трубы.
//!
//! - `protocol`: сообщения JSON + бинарные пакеты с шапкой;
//! - `server`: TCP, веб-сокеты, unix-сокет / именованный пайп поверх демона.

pub mod protocol;
pub mod server;

pub use protocol::{ClientMsg, ServerMsg};
pub use server::Serve;
