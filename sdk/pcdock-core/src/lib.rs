//! Ядро PC Dock. Не знает об ОС и не выполняет ввод-вывод: байты и USB-запросы
//! приходят и уходят через явные вызовы (подход sans-IO).
#![forbid(unsafe_code)]

pub mod hid;
pub mod input;
pub mod packet;
pub mod session;
