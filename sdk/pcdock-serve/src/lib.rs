//! Связь с демоном: один протокол, четыре трубы.
//!
//! - `protocol`: сообщения JSON + бинарные пакеты с шапкой;
//! - `server`: TCP, веб-сокеты, unix-сокет / именованный пайп поверх демона.

pub mod files;
pub mod protocol;
pub mod server;

pub use files::{FileBackend, FileOptions, FileWriter};
pub use protocol::{ClientMsg, ServerMsg};
pub use server::{Serve, PROTOCOL_VERSION};
