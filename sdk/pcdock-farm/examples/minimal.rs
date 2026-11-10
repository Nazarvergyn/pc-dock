//! Минимальный потребитель фермы: печатает события и нажимает в центре экрана первого готового телефона.
//! Запуск: `PCDOCK_SERVER_APK=.../server-adb-debug.apk cargo run -p pcdock-farm --example minimal`

use std::time::Duration;

use pcdock_core::input::TouchPhase;
use pcdock_core::link::{InputCmd, PhoneState};
use pcdock_farm::{Farm, FarmConfig, FarmEvent};

fn main() -> Result<(), String> {
    // Автообнаружение USB и ADB; APK сервера берётся из PCDOCK_SERVER_APK.
    let (mut farm, warnings) = Farm::from_config(FarmConfig::default())?;
    for w in warnings {
        eprintln!("предупреждение: {w}");
    }
    let mut tapped = false;
    loop {
        for ev in farm.drain() {
            match ev {
                FarmEvent::Appeared { phone, transport, serial, .. } => {
                    println!("телефон {phone}: {transport} {serial:?}");
                    farm.set_watching(phone, true)?; // хотим видео от этого телефона
                }
                FarmEvent::State { phone, state, detail } => println!("{phone}: {state:?} {detail}"),
                FarmEvent::Video { phone, frame } => {
                    println!("{phone}: кадр {} ({} байт)", frame.seq, frame.data.len())
                }
                FarmEvent::Lost { phone } => println!("{phone}: потерян"),
                _ => {}
            }
        }
        if !tapped {
            if let Some(&id) = farm.phones().first() {
                if farm.state(id) == Ok(PhoneState::Ready) {
                    farm.command(id, InputCmd::DeviceEnable { kind: pcdock_core::input::DeviceKind::Touch, on: true })?;
                    farm.command(id, InputCmd::touch_ex(0, TouchPhase::Down, 0.5, 0.5, None, None, None))?;
                    farm.command(id, InputCmd::touch_ex(0, TouchPhase::Up, 0.5, 0.5, None, None, None))?;
                    tapped = true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
