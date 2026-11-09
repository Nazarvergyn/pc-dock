//! Готовая настройка фермы с USB и ADB: из `FarmConfig` получается ферма с обнаружением и фабрикой каналов.
//! Демон и C ABI превращают свои входы (аргументы, JSON) в `FarmConfig` и больше ничего не решают.

use std::time::Duration;

use pcdock_core::link::{Discovery, LinkFactory, PhoneLink, Sighting, Transport};
use pcdock_core::packet::SetStream;
use pcdock_core::session::Config;
use pcdock_usb::{PhoneSelector, Settings, UsbLink, UsbPort, ANDROID_VIDS};

use crate::runtime::{DiscoveryConfig, Farm, PhoneSpec, Rules};

/// Настройка фермы.
#[derive(Clone, Debug)]
pub struct FarmConfig {
    /// Имя хоста в HELLO.
    pub name: String,
    /// Видео и звук по умолчанию для новых телефонов.
    pub video: bool,
    pub audio: bool,
    /// Брать всё найденное (кроме `except`).
    pub auto: bool,
    /// Исключения для `auto` (см. [`Rules`]).
    pub except: Vec<String>,
    /// Закреплённые телефоны: принимаются всегда, транспорт задан правилом.
    pub pinned: Vec<PhoneSpec>,
    /// Какие семейства искать.
    pub usb: bool,
    pub adb: bool,
    pub serial: bool,
    /// Производители USB (VID), которые считаются телефонами.
    pub vendor_ids: Vec<u16>,
    /// Адрес ADB-сервера; `None` — локальный.
    pub adb_server: Option<String>,
    /// Первый локальный порт для проброса ADB; следующие телефоны берут порты выше.
    pub adb_local_port: u16,
    pub every: Duration,
    pub lost_after: Duration,
    /// Предпочтение транспортов, если один телефон виден по нескольким.
    pub order: Vec<Transport>,
}

impl Default for FarmConfig {
    fn default() -> Self {
        let d = DiscoveryConfig::default();
        Self {
            name: "pcdock/1.0".into(),
            video: true,
            audio: false,
            auto: true,
            except: Vec::new(),
            pinned: Vec::new(),
            usb: true,
            adb: true,
            serial: true,
            vendor_ids: ANDROID_VIDS.to_vec(),
            adb_server: None,
            adb_local_port: 47000,
            every: d.every,
            lost_after: d.lost_after,
            order: d.order,
        }
    }
}

impl FarmConfig {
    fn session(&self) -> Config {
        Config {
            name: self.name.clone(),
            stream: Some(SetStream { video: self.video, audio: self.audio, ..Default::default() }),
            ..Default::default()
        }
    }
}

impl Farm {
    /// Ферма по настройке. Возвращает предупреждения запуска (например, USB-обнаружение не встало):
    /// ферма при этом работает с тем, что удалось поднять.
    pub fn from_config(cfg: FarmConfig) -> Result<(Farm, Vec<String>), String> {
        if !cfg.auto && cfg.pinned.is_empty() {
            return Err("нужно автообнаружение или хотя бы один закреплённый телефон".into());
        }
        let mut warnings = Vec::new();
        let mut providers: Vec<Box<dyn Discovery>> = Vec::new();
        if cfg.usb {
            match pcdock_usb::UsbDiscovery::new(cfg.vendor_ids.clone()) {
                Ok(d) => providers.push(Box::new(d)),
                Err(e) => warnings.push(format!("USB-обнаружение не встало: {e}")),
            }
        }
        if cfg.adb {
            let adb = match &cfg.adb_server {
                Some(addr) => pcdock_adb::Adb::new(addr.clone()),
                None => pcdock_adb::Adb::local(),
            };
            providers.push(Box::new(pcdock_adb::AdbDiscovery::new(adb)));
        }
        if cfg.serial {
            warnings.push("serial-трубы: транспорт не готов, пропускаю".into());
        }
        let discovery = DiscoveryConfig {
            rules: Rules { auto: cfg.auto, except: cfg.except.clone(), pinned: cfg.pinned.clone() },
            every: cfg.every,
            lost_after: cfg.lost_after,
            order: cfg.order.clone(),
        };
        let factory = DefaultFactory { session: cfg.session(), next_adb_port: cfg.adb_local_port };
        let mut farm = Farm::new();
        farm.enable_discovery(providers, Box::new(factory), discovery);
        Ok((farm, warnings))
    }
}

/// Создаёт канал по факту обнаружения: USB-аксессуар или ADB.
struct DefaultFactory {
    session: Config,
    next_adb_port: u16,
}

impl LinkFactory for DefaultFactory {
    fn make(&mut self, s: &Sighting) -> Result<Box<dyn PhoneLink>, String> {
        match s.transport {
            Transport::Usb => {
                let port: UsbPort = s.address.parse()?;
                let target = PhoneSelector { port: Some(port), serial: s.serial.clone() };
                let link = UsbLink::new(
                    self.session.clone(),
                    Settings { target: Some(target), ..Settings::default() },
                )?;
                Ok(Box::new(link))
            }
            Transport::Adb => {
                let local = self.next_adb_port;
                self.next_adb_port = self.next_adb_port.checked_add(1).unwrap_or(47000);
                Ok(Box::new(pcdock_adb::AdbLink::new(self.session.clone(), s.address.clone(), local)))
            }
            Transport::Serial => Err("serial-транспорт не готов".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_needs_auto_or_pinned() {
        let none = FarmConfig { auto: false, usb: false, adb: false, serial: false, ..FarmConfig::default() };
        assert!(Farm::from_config(none).is_err());
        let pinned = FarmConfig {
            auto: false,
            usb: false,
            adb: false,
            serial: false,
            pinned: vec![PhoneSpec::parse("adb:ABC").unwrap()],
            ..FarmConfig::default()
        };
        let (farm, _) = Farm::from_config(pinned).unwrap();
        assert!(farm.phones().is_empty(), "ферма стартует пустой");
    }
}
