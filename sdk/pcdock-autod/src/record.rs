//! Запись экрана прогона и журналы телефона как вложения прогона.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::backend::Backend;
use crate::events::Sink;

/// Идущая запись одного телефона.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    frames: PathBuf,
    out: PathBuf,
    fps: f64,
}

/// Начинает запись: кадры идут в `<папка>/rec-<телефон>/NNNNN.jpg` не чаще `fps` в секунду.
pub fn start(backend: Arc<dyn Backend>, phone: u32, fps: f64, dir: &Path) -> Recorder {
    let frames = dir.join(format!("rec-{phone}"));
    let stop = Arc::new(AtomicBool::new(false));
    let (f2, s2) = (frames.clone(), stop.clone());
    let thread = std::thread::Builder::new()
        .name(format!("rec-{phone}"))
        .spawn(move || {
            if fs::create_dir_all(&f2).is_err() || backend.acquire(phone).is_err() {
                return;
            }
            let step = Duration::from_secs_f64(1.0 / fps);
            let (mut n, mut last_seq, mut next) = (0u32, None, Instant::now());
            while !s2.load(Ordering::SeqCst) {
                if let Some(fr) = backend.wait_frame(phone, last_seq, Duration::from_millis(500)) {
                    if Instant::now() >= next {
                        n += 1;
                        let _ = fs::write(f2.join(format!("{n:05}.jpg")), fr.jpeg.as_slice());
                        next = Instant::now() + step;
                    }
                    last_seq = Some(fr.seq + 1);
                }
            }
            backend.release(phone);
        })
        .ok();
    Recorder { stop, thread, frames, out: dir.join(format!("rec-{phone}.mp4")), fps }
}

impl Recorder {
    /// Останавливает запись. Возвращает путь к mp4, если собрать его удалось; иначе кадры остаются JPEG-файлами.
    pub fn finish(mut self) -> Result<Option<PathBuf>, String> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let has_frames = fs::read_dir(&self.frames).map(|d| d.count() > 0).unwrap_or(false);
        if !has_frames {
            return Err("кадров не было".into());
        }
        let status = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-framerate"])
            .arg(self.fps.to_string())
            .arg("-i")
            .arg(self.frames.join("%05d.jpg"))
            .args(["-pix_fmt", "yuv420p"])
            .arg(&self.out)
            .status();
        match status {
            Ok(s) if s.success() => {
                let _ = fs::remove_dir_all(&self.frames);
                Ok(Some(self.out.clone()))
            }
            Ok(_) => Err(format!("ffmpeg не собрал видео, кадры в {}", self.frames.display())),
            Err(_) => Err(format!("ffmpeg не найден, кадры в {}", self.frames.display())),
        }
    }
}

/// `logcat` телефона и `ExitInfo` по пакетам: файлы в папке прогона. Сбой одного не мешает остальным.
pub fn save_device_logs(
    backend: &dyn Backend,
    phone: u32,
    packages: &[String],
    dir: &Path,
    sink: &Arc<dyn Sink>,
) {
    let save = |name: String, op: serde_json::Value| match backend.device(phone, op) {
        Ok(v) => {
            let text = v["text"].as_str().unwrap_or("");
            if let Err(e) = fs::write(dir.join(&name), text) {
                sink.emit("log", json!({"level": "warn", "text": format!("{name}: {e}")}));
            }
        }
        Err(e) => sink.emit("log", json!({"level": "warn", "text": format!("телефон {phone}: {name}: {e}")})),
    };
    save(format!("phone-{phone}-logcat.txt"), json!({"op": "Logcat", "lines": 2000}));
    for p in packages {
        save(format!("phone-{phone}-exit-{p}.txt"), json!({"op": "ExitInfo", "package": p}));
    }
}
