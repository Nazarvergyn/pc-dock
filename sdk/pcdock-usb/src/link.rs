//! `UsbLink`: [`Dock`](crate::Dock) как транспорт для фермы.

use pcdock_core::input::TouchPoint;
use pcdock_core::link::{InputCmd, LinkError, PhoneLink};
use pcdock_core::packet::SetStream;
use pcdock_core::session::Event;

/// USB-связь поверх готового `Dock`: одна трубка, ретраи снаружи в воркере.
pub struct UsbLink {
    dock: crate::Dock,
}

impl UsbLink {
    pub fn new(config: pcdock_core::session::Config, settings: crate::Settings) -> Result<Self, String> {
        crate::Dock::new(config, settings).map(|dock| Self { dock }).map_err(|e| e.to_string())
    }
}

impl PhoneLink for UsbLink {
    fn is_connected(&self) -> bool {
        self.dock.is_connected()
    }

    fn connect(&mut self) -> Result<(), LinkError> {
        self.dock.connect().map_err(|e| classify_usb(&e))
    }

    fn poll(&mut self) -> Result<Vec<Event>, LinkError> {
        let mut events = self.dock.poll().map_err(|e| classify_usb(&e))?;
        for w in self.dock.take_warnings() {
            events.push(Event::Status(pcdock_core::packet::Status { code: 0, detail: w }));
        }
        Ok(events)
    }

    fn input(&mut self, cmd: &InputCmd) -> Result<(), LinkError> {
        let s = self.dock.session();
        let r = match cmd {
            InputCmd::DeviceEnable { kind, on } => {
                s.device_enable(*kind, *on);
                Ok(())
            }
            InputCmd::Touch { cid, phase, x, y, width, height, pressure } => {
                let mut pt = TouchPoint::new(*cid, *phase, *x, *y);
                pt.width = *width;
                pt.height = *height;
                pt.pressure = *pressure;
                s.touch(pt).map_err(|e| e.to_string())
            }
            InputCmd::CursorMoveAbs { x, y } => s.cursor_move_abs(*x, *y).map_err(|e| e.to_string()),
            InputCmd::CursorMoveRel { dx, dy } => s.cursor_move_rel(*dx, *dy).map_err(|e| e.to_string()),
            InputCmd::CursorResync => {
                s.cursor_resync();
                Ok(())
            }
            InputCmd::MouseButton { button, down } => {
                s.mouse_button(*button, *down).map_err(|e| e.to_string())
            }
            InputCmd::Scroll { ticks } => s.scroll(*ticks).map_err(|e| e.to_string()),
            InputCmd::ScrollH { ticks } => s.scroll_h(*ticks).map_err(|e| e.to_string()),
            InputCmd::Key { usage, down } => s.key(*usage, *down).map_err(|e| e.to_string()),
            InputCmd::ReleaseAll => {
                s.release_all();
                Ok(())
            }
            InputCmd::RequestFrame => {
                s.request_frame();
                Ok(())
            }
            InputCmd::Wake => {
                s.wake();
                Ok(())
            }
            InputCmd::SetStream { video, audio } => {
                let st = SetStream { video: *video, audio: *audio, ..SetStream::default() };
                s.set_stream(st);
                Ok(())
            }
        };
        r.map_err(|e| LinkError::Fatal(e.to_string()))
    }

    fn shutdown(&mut self) {
        self.dock.shutdown();
    }
}

fn classify_usb(e: &crate::Error) -> LinkError {
    match e {
        crate::Error::Usb(_) | crate::Error::NoDevice | crate::Error::HandshakeTimeout => {
            LinkError::Gone(e.to_string())
        }
        _ => LinkError::Fatal(e.to_string()),
    }
}
