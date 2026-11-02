"""Воспроизведение звука телефона на ПК (протокол v2: AUDIO_CONFIG / AUDIO_FRAME, PCM s16le).

Без новых зависимостей: играет через pygame.mixer (SDL). Куски ~20мс проигрываются
как короткие Sound-объекты; при переполнении каналов старые куски отбрасываются.
Если аудиоустройства нет (например, WSL без звука) — тихо деградирует в счётчик.
"""
import time

import pygame


class AudioPlayer:
    def __init__(self, log=print):
        self.log = log
        self.sample_rate = 0
        self.channels = 0
        self.ready = False
        self.volume = 1.0
        self.frames = 0
        self.dropped = 0
        self.bytes = 0
        self._last_log = 0.0

    def set_config(self, sample_rate, channels):
        if sample_rate == self.sample_rate and channels == self.channels and self.ready:
            return
        self.close()
        self.sample_rate = sample_rate
        self.channels = channels
        try:
            pygame.mixer.init(frequency=sample_rate, size=-16, channels=channels, buffer=1024)
            self.ready = True
            self.log(f"[audio] выход: PCM {sample_rate} Гц, каналов: {channels}")
        except Exception as e:
            self.log(f"[audio] не удалось открыть устройство вывода: {e!r} (звук считаю, но не играю)")
            self.ready = False

    def set_volume(self, v: float):
        self.volume = max(0.0, min(1.0, float(v)))

    def feed(self, pcm: bytes):
        self.frames += 1
        self.bytes += len(pcm)
        if not self.ready:
            return
        try:
            snd = pygame.mixer.Sound(buffer=pcm)
            snd.set_volume(self.volume)
            ch = snd.play()
            if ch is None:
                self.dropped += 1
        except Exception:
            self.dropped += 1
        now = time.time()
        if now - self._last_log >= 5 and self.frames:
            self._last_log = now
            self.log(f"[audio] кадров {self.frames}, {self.bytes} Б, отброшено {self.dropped}")

    def close(self):
        try:
            if self.ready:
                pygame.mixer.quit()
        except Exception:
            pass
        self.ready = False
