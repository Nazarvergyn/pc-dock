"""Окно просмотра экрана телефона и пересылка ввода через Rust-ядро (SDK).

Протокола и HID на Питоне нет: ввод — это команды сессии (`sdk.SdkSession`),
отчёты и регистрацию строит ядро, Питон только выполняет транспорт в main.py."""
import time

import cv2
import pygame

from sdk import (
    BTN_LEFT,
    BTN_MIDDLE,
    BTN_RIGHT,
    DEV_KEYBOARD,
    DEV_MOUSE,
    DEV_TOUCH,
    SdkError,
    TOUCH_DOWN,
    TOUCH_MOVE,
    TOUCH_UP,
)

# Множитель чувствительности колеса (дробь копится внутри ядра)
SCROLL_GAIN = 1.0

# Панель с кнопками включения устройств ввода внизу окна
TOOLBAR_H = 54
DEVICES = ("touch", "mouse", "keyboard")
TITLES = {"touch": "Касание", "mouse": "Мышь", "keyboard": "Клавиатура"}
# Горячие клавиши включения/отключения (на телефон не отправляются)
TOGGLE_KEYS = {pygame.K_F10: "touch", pygame.K_F11: "mouse", pygame.K_F12: "keyboard"}
KEY_NAMES = {"touch": "F10", "mouse": "F11", "keyboard": "F12"}
# Звук: F9 — вкл/выкл (на телефон не отправляется, шлёт SET_STREAM), колесо над кнопкой — громкость
AUDIO_KEY = pygame.K_F9
AUDIO_KEY_NAME = "F9"
# Побудка: F8 — включить экран телефона на минуту (ввести PIN), шлёт WAKE
WAKE_KEY = pygame.K_F8
WAKE_KEY_NAME = "F8"

COLOR_BG = (28, 29, 33)
COLOR_ON = (38, 132, 70)
COLOR_ON_HOVER = (48, 156, 84)
COLOR_OFF = (88, 52, 52)
COLOR_OFF_HOVER = (112, 64, 64)
COLOR_TEXT = (240, 240, 240)
COLOR_SUBTEXT = (200, 215, 205)
COLOR_ACTION = (70, 90, 130)
COLOR_ACTION_HOVER = (88, 112, 155)


def load_font(size):
    """Шрифт с кириллицей: ищем в системе, иначе встроенный."""
    for name in ("dejavusans", "liberationsans", "notosans", "arial", "freesans"):
        path = pygame.font.match_font(name)
        if path:
            return pygame.font.Font(path, size)
    return pygame.font.Font(None, size + 6)


class PhoneWindow:
    def __init__(self, session, log=print, enabled=DEVICES, audio=None, audio_on=True, on_audio=None):
        self.session = session
        self.log = log
        # Активные устройства ввода. touch/mouse — радио, keyboard — независимо.
        self.active = {name: False for name in DEVICES}
        # Звук: audio — плеер ПК, on_audio(on) — колбэк отправки SET_STREAM на телефон.
        self.audio = audio
        self.audio_on = bool(audio_on) and audio is not None
        self._on_audio = on_audio
        # touch и mouse взаимоисключающие (режимы указателя): оба сразу включать нельзя.
        # Если просили оба (старт по умолчанию) — оставляем touch, mouse остаётся выкл.
        enabled = tuple(enabled)
        if "touch" in enabled and "mouse" in enabled:
            enabled = tuple(n for n in enabled if n != "mouse")
            self.log("[hid] touch и mouse взаимоисключающие: включён touch (мышь — по F11)")
        for name in enabled:
            if name in self.active:
                self.active[name] = True
        if not self.active["touch"] and not self.active["mouse"]:
            self.log("[!] touch и mouse выключены — указателя нет. F10 — касание, F11 — мышь")

        pygame.init()
        self.screen = pygame.display.set_mode((360, 820 + TOOLBAR_H), pygame.RESIZABLE)
        pygame.display.set_caption("Phone Screen")
        self.font = load_font(17)
        self.font_small = load_font(13)
        self.surface = None
        self.fw = self.fh = 0
        self.dirty = True
        self.last_frac = None
        self._touch_down = False  # палец на экране (между press и release)
        self.last_error_log = 0.0
        self.pointer = (-1, -1)  # последнее положение курсора ПК в окне
        self.hover = None  # кнопка панели под курсором
        self._wheel_v = 0.0  # накопленный скролл: сливаем тики кадра в 1 репорт
        self._wheel_h = 0.0  # иначе каждый тик = блокирующий control transfer и видео встаёт
        self._cursor_state = None  # hidden | hand | arrow — что реально выставлено
        self._empty_cursor = None  # прозрачный курсор 8x8 (запасной путь прятания)
        self.frame_no = 0  # счётчик кадров для экранного статуса (стоит — цикл/видео встали)
        self.last_frame_t = 0.0
        self._setup_cursors()
        self._apply_cursor()

    # Активно ли устройство ввода
    @property
    def touch(self):
        return self.active["touch"]

    @property
    def mouse(self):
        return self.active["mouse"]

    @property
    def keyboard(self):
        return self.active["keyboard"]

    # ---------- включение и отключение устройств (команды ядру; транспорт — в main.py) ----------
    _KIND = {"touch": DEV_TOUCH, "mouse": DEV_MOUSE, "keyboard": DEV_KEYBOARD}

    def _enable(self, name):
        try:
            self.session.device_enable(self._KIND[name], True)
        except SdkError as e:
            self.log(f"[!] Не удалось включить {name}: {e}")
            return
        # Режимы указателя — радио: второй гасим ТОЛЬКО после успешного включения
        # нового, иначе при ошибке останемся вообще без указателя.
        if name == "touch" and self.active["mouse"]:
            self._disable("mouse")
        elif name == "mouse" and self.active["touch"]:
            self._disable("touch")
        if name == "mouse":
            try:
                self.session.cursor_resync()
            except SdkError as e:
                self.log(f"[!] resync мыши: {e}")
        self.active[name] = True
        self.log(f"[hid] {name} включён")

    def _disable(self, name):
        if not self.active[name]:
            return
        # Ядро само отпускает нажатое и снимает устройство с телефона.
        try:
            self.session.device_enable(self._KIND[name], False)
        except SdkError as e:
            self.log(f"[!] Не удалось выключить {name}: {e}")
            return
        self.active[name] = False
        self.log(f"[hid] {name} отключён")

    def _toggle(self, name):
        if name == "audio":
            self._toggle_audio()
            return
        if name == "wake":
            self._wake_phone()
            return
        if name in ("touch", "mouse"):
            # Радио: один указатель всегда включён, «выключить оба» нельзя.
            # Повторное нажатие активного — игнор с подсказкой, переключает только другой.
            if self.active[name]:
                other = "mouse" if name == "touch" else "touch"
                self.log(f"[hid] {name} уже включён — переключение: {KEY_NAMES[other]}")
                return
            self._enable(name)
            self.dirty = True
            self._apply_cursor()
            return
        if self.active[name]:
            self._disable(name)
        else:
            self._enable(name)
        self.dirty = True
        self._apply_cursor()

    def _wake_phone(self):
        """Разбудить экран телефона на минуту (ввести PIN). Моментальное действие, не тумблер."""
        try:
            self.session.wake()
            self.log("[wake] запрос отправлен (экран включится на минуту)")
        except SdkError as e:
            self.log(f"[!] Не удалось разбудить: {e}")

    def _toggle_audio(self):
        """Вкл/выкл звука: сообщаем телефону через SET_STREAM (он стартует/останавливает захват)."""
        self.audio_on = not self.audio_on
        self.log(f"[audio] {'включён' if self.audio_on else 'выключен'} (F9)")
        if self._on_audio is not None:
            try:
                self._on_audio(self.audio_on)
            except Exception as e:
                self.log(f"[!] Не удалось переключить звук: {e!r}")
        self.dirty = True

    def _audio_volume(self, delta):
        if self.audio is None:
            return
        self.audio.set_volume(self.audio.volume + delta)
        self.log(f"[audio] громкость {int(self.audio.volume * 100)}%")
        self.dirty = True

    # ---------- курсор ПК ----------
    def _setup_cursors(self):
        """Прозрачный курсор 8x8 — второй путь прятания: если set_visible(False)
        окружение игнорирует, прозрачная форма всё равно ничего не рисует."""
        try:
            self._empty_cursor = ((8, 8), (0, 0), (0,) * 8, (0,) * 8)
        except Exception as e:
            self._empty_cursor = None
            self.log(f"[!] Прозрачный курсор не встал: {e!r}")

    def _apply_cursor(self):
        """Курсор ОС скрыт над картинкой в обоих режимах: в мыши рисует телефон,
        в касании рисуем свой кружок в draw(). Один курсор, не два.
        Прячем двумя путями сразу: set_visible(False) + прозрачная форма."""
        over_image = self._frac(self.pointer) is not None
        if over_image and (self.mouse or self.touch):
            want = "hidden"
        elif self._button_at(self.pointer):
            want = "hand"
        else:
            want = "arrow"
        if want == self._cursor_state:
            if want == "hidden":
                # Дублируем каждый кадр: если WM вернул курсор между событиями — снова прячем.
                pygame.mouse.set_visible(False)
            return
        self._cursor_state = want
        if want == "hidden":
            pygame.mouse.set_visible(False)
            if self._empty_cursor is not None:
                try:
                    pygame.mouse.set_cursor(*self._empty_cursor)
                except Exception as e:
                    # Видно один раз при переходе: если прозрачный курсор не встал —
                    # прятание держится только на set_visible, и это надо знать.
                    self.log(f"[!] Прозрачный курсор не применился: {e!r}")
                    self._empty_cursor = None
            self.log("[cursor] курсор ОС скрыт")
        else:
            pygame.mouse.set_visible(True)
            try:
                pygame.mouse.set_cursor(pygame.SYSTEM_CURSOR_HAND if want == "hand"
                                        else pygame.SYSTEM_CURSOR_ARROW)
            except Exception as e:
                self.log(f"[!] Форма курсора не применилась: {e!r}")
            self.log("[cursor] курсор ОС видим")

    # ---------- отрисовка ----------
    def update_frame(self, bgr):
        h, w = bgr.shape[:2]
        rgb = cv2.cvtColor(bgr, cv2.COLOR_BGR2RGB)
        self.surface = pygame.image.frombuffer(rgb.tobytes(), (w, h), "RGB")
        self.fw, self.fh = w, h
        self.frame_no += 1
        self.last_frame_t = time.time()
        self.dirty = True

    def _image_area(self):
        ww, wh = self.screen.get_size()
        return ww, max(1, wh - TOOLBAR_H)

    def _rect(self):
        """Прямоугольник картинки в окне (над панелью) с сохранением пропорций"""
        aw, ah = self._image_area()
        if not self.fw or not self.fh:
            return 0, 0, aw, ah
        scale = min(aw / self.fw, ah / self.fh)
        w, h = int(self.fw * scale), int(self.fh * scale)
        return (aw - w) // 2, (ah - h) // 2, w, h

    def _toolbar_names(self):
        """Кнопки панели: HID-устройства + звук (если есть плеер) + побудка."""
        names = list(DEVICES)
        if self.audio is not None:
            names.append("audio")
        names.append("wake")
        return names

    def _buttons(self):
        """Прямоугольники кнопок панели: [(имя, pygame.Rect)]"""
        names = self._toolbar_names()
        ww, wh = self.screen.get_size()
        gap = 6
        width = (ww - gap * (len(names) + 1)) // len(names)
        top = wh - TOOLBAR_H + 6
        return [
            (name, pygame.Rect(gap + i * (width + gap), top, width, TOOLBAR_H - 12))
            for i, name in enumerate(names)
        ]

    def _button_at(self, pos):
        for name, rect in self._buttons():
            if rect.collidepoint(pos):
                return name
        return None

    def _draw_toolbar(self):
        ww, wh = self.screen.get_size()
        pygame.draw.rect(self.screen, COLOR_BG, (0, wh - TOOLBAR_H, ww, TOOLBAR_H))
        for name, rect in self._buttons():
            if name == "wake":
                on = True  # моментальное действие, не тумблер
                title = self.font.render("Будить", True, COLOR_TEXT)
                state = self.font_small.render(f"{WAKE_KEY_NAME}", True, COLOR_SUBTEXT)
                hover = self.hover == name
                color = COLOR_ACTION_HOVER if hover else COLOR_ACTION
                pygame.draw.rect(self.screen, color, rect, border_radius=8)
                self.screen.blit(title, title.get_rect(midtop=(rect.centerx, rect.y + 7)))
                self.screen.blit(state, state.get_rect(midbottom=(rect.centerx, rect.bottom - 6)))
                continue
            if name == "audio":
                on = self.audio_on
                title = self.font.render("Звук", True, COLOR_TEXT)
                vol = int(self.audio.volume * 100) if self.audio is not None else 100
                state = self.font_small.render(f"{'вкл' if on else 'выкл'} {vol}% · {AUDIO_KEY_NAME}", True, COLOR_SUBTEXT)
            else:
                on = self.active[name]
                title = self.font.render(TITLES[name], True, COLOR_TEXT)
                state = self.font_small.render(f"{'вкл' if on else 'выкл'} · {KEY_NAMES[name]}", True, COLOR_SUBTEXT)
            hover = self.hover == name
            color = (COLOR_ON_HOVER if hover else COLOR_ON) if on else (COLOR_OFF_HOVER if hover else COLOR_OFF)
            pygame.draw.rect(self.screen, color, rect, border_radius=8)
            self.screen.blit(title, title.get_rect(midtop=(rect.centerx, rect.y + 7)))
            self.screen.blit(state, state.get_rect(midbottom=(rect.centerx, rect.bottom - 6)))

    def _draw_status(self):
        """Живой статус прямо на картинке: режим, счётчик кадров, возраст кадра, курсор, звук.
        Счётчик стоит — цикл/видео встали (видно без логов)."""
        mode = "touch" if self.touch else ("mouse" if self.mouse else "none")
        age = time.time() - self.last_frame_t if self.last_frame_t > 0 else -1
        cur = self._cursor_state or "?"
        snd = ("on" if self.audio_on else "off") if self.audio is not None else "-"
        txt = f"{mode} f{self.frame_no} {age:.1f}s cur:{cur} a:{snd}" if age >= 0 else f"{mode} no video cur:{cur} a:{snd}"
        try:
            img = self.font_small.render(txt, True, (255, 255, 255))
            shadow = self.font_small.render(txt, True, (0, 0, 0))
            self.screen.blit(shadow, (9, 9))
            self.screen.blit(img, (8, 8))
        except Exception:
            pass

    def draw(self):
        if not self.dirty:
            return
        self.screen.fill((0, 0, 0))
        if self.surface is not None:
            x, y, w, h = self._rect()
            self.screen.blit(pygame.transform.scale(self.surface, (w, h)), (x, y))
            # В касании телефон курсора не показывает — рисуем свой кружок.
            # В мыши курсор рисует телефон, ничего не рисуем.
            if self.touch and self._frac(self.pointer) is not None:
                pygame.draw.circle(self.screen, (255, 255, 255), self.pointer, 6, 1)
                pygame.draw.circle(self.screen, (255, 255, 255), self.pointer, 1)
            self._draw_status()
        self._draw_toolbar()
        pygame.display.flip()
        self.dirty = False

    # ---------- ввод ----------
    def _frac(self, pos):
        x, y, w, h = self._rect()
        if w < 2 or h < 2:
            return None
        px, py = pos[0] - x, pos[1] - y
        if not (0 <= px < w and 0 <= py < h) or pos[1] >= self._image_area()[1]:
            return None
        return px / (w - 1), py / (h - 1)

    def _release_all(self):
        try:
            self.session.release_all()
        except SdkError as e:
            self.log(f"[!] release_all: {e}")

    def _handle(self, e):
        if e.type == pygame.MOUSEMOTION:
            self.pointer = e.pos
            hover = self._button_at(e.pos)
            if hover != self.hover:
                self.hover = hover
                self.dirty = True
            self._apply_cursor()
            frac = self._frac(e.pos)
            if frac is None:
                return
            self.last_frac = frac
            # Кружок вместо курсора ОС должен двигаться за мышью -> перерисовка.
            if self.touch and not pygame.mouse.get_visible():
                self.dirty = True
            if self.touch:
                if self._touch_down:
                    self.session.touch(0, TOUCH_MOVE, *frac)
            elif self.mouse:
                # Ховер в режиме мыши: тянем курсор Android за курсором ОС.
                # Первое движение после входа/включения делает resync()
                # (прижать к углу и отсчитать заново), иначе курсор "уплывает".
                self.session.cursor_move_abs(*frac)
        elif e.type == pygame.MOUSEBUTTONDOWN or e.type == pygame.MOUSEBUTTONUP:
            pressed = e.type == pygame.MOUSEBUTTONDOWN
            button = self._button_at(e.pos)
            if button and e.button == 1:
                if pressed:
                    self._toggle(button)
                return
            frac = self._frac(e.pos)
            if e.button == 1 and self.touch:
                if pressed and frac:
                    self.last_frac = frac
                    self._touch_down = True
                    self.session.touch(0, TOUCH_DOWN, *frac)
                elif not pressed and self.last_frac:
                    self._touch_down = False
                    self.session.touch(0, TOUCH_UP, *(frac or self.last_frac))
            elif e.button == 1 and self.mouse and (frac or not pressed):
                # В режиме мыши левая кнопка — это левая кнопка мыши,
                # а не тап (тап — только в режиме дигитайзера).
                if frac:
                    self.session.cursor_move_abs(*frac)
                self.session.mouse_button(BTN_LEFT, pressed)
            elif e.button in (2, 3) and self.mouse and (frac or not pressed):
                if frac:
                    self.session.cursor_move_abs(*frac)
                self.session.mouse_button(BTN_MIDDLE if e.button == 2 else BTN_RIGHT, pressed)
        elif e.type == pygame.MOUSEWHEEL:
            # Только копим: отправка — раз в кадр в _flush_wheel().
            # Иначе каждый тик = отдельный блокирующий control transfer
            # посреди цикла main.py и видео встаёт на время скролла.
            if self.last_frac is None:
                return
            if self._button_at(self.pointer) == "audio":
                if e.y:
                    self._audio_volume(0.1 if e.y > 0 else -0.1)
                return
            if not (self.mouse or self.touch):
                return
            if e.y:
                self._wheel_v += e.y * SCROLL_GAIN
            if e.x:
                self._wheel_h += e.x * SCROLL_GAIN
        elif e.type in (pygame.KEYDOWN, pygame.KEYUP) and e.key == WAKE_KEY:
            if e.type == pygame.KEYDOWN:
                self._wake_phone()
        elif e.type in (pygame.KEYDOWN, pygame.KEYUP) and e.key == AUDIO_KEY:
            if e.type == pygame.KEYDOWN:
                self._toggle_audio()
        elif e.type in (pygame.KEYDOWN, pygame.KEYUP) and e.key in TOGGLE_KEYS:
            if e.type == pygame.KEYDOWN:
                self._toggle(TOGGLE_KEYS[e.key])
        elif e.type in (pygame.KEYDOWN, pygame.KEYUP) and self.keyboard \
                and e.key not in TOGGLE_KEYS and e.key not in (AUDIO_KEY, WAKE_KEY):
            self.session.key(e.scancode, e.type == pygame.KEYDOWN)
        elif e.type == pygame.WINDOWENTER:
            # Курсор ОС только что зашёл в окно: забываем оценку позиции Android,
            # следующее движение прижмёт курсор к углу и поставит в точку входа.
            # Без этого курсор Android появляется не под курсором ОС, а со сдвигом.
            if self.mouse:
                try:
                    self.session.cursor_resync()
                except SdkError as e:
                    self.log(f"[!] resync мыши: {e}")
            self._apply_cursor()
            self.dirty = True
        elif e.type in (pygame.WINDOWLEAVE, pygame.WINDOWFOCUSLOST):
            self._release_all()
            if e.type == pygame.WINDOWLEAVE:
                self.pointer = (-1, -1)
                self.hover = None
                self.dirty = True
                self._apply_cursor()
        elif e.type in (pygame.WINDOWRESIZED, pygame.WINDOWEXPOSED, pygame.VIDEORESIZE):
            self.dirty = True
            self._apply_cursor()

    def _flush_wheel(self):
        """Отправка накопленного за кадр скролла (макс 1 связка репортов на кадр)."""
        v, h = self._wheel_v, self._wheel_h
        self._wheel_v, self._wheel_h = 0.0, 0.0
        if (v == 0 and h == 0) or self.last_frac is None:
            return
        if self.mouse:
            # Колесо крутится там, куда смотрит курсор ПК: сначала подтягиваем
            # курсор Android в точку, иначе скролл уйдёт в чужое окно.
            try:
                self.session.cursor_move_abs(*self.last_frac)
                if v:
                    self.session.scroll(v)
                if h:
                    self.session.scroll_h(h)
            except SdkError as err:
                now = time.time()
                if now - self.last_error_log > 2:
                    self.last_error_log = now
                    self.log(f"[!] Ошибка скролла: {err}")
        elif self.touch and not self._touch_down:
            # В таче колеса нет: эмулируем свайп от точки курсора.
            # Вверх колеса (v>0) = свайп вверх (палец едет вверх).
            try:
                x, y = self.last_frac
                dy = max(-0.5, min(0.5, -v * 0.12))
                dx = max(-0.5, min(0.5, -h * 0.12))
                if dx == 0 and dy == 0:
                    return
                ex = max(0.0, min(1.0, x + dx))
                ey = max(0.0, min(1.0, y + dy))
                mx = (x + ex) / 2
                my = (y + ey) / 2
                self.session.touch(0, TOUCH_DOWN, x, y)
                self.session.touch(0, TOUCH_MOVE, mx, my)
                self.session.touch(0, TOUCH_MOVE, ex, ey)
                self.session.touch(0, TOUCH_UP, ex, ey)
            except SdkError as err:
                now = time.time()
                if now - self.last_error_log > 2:
                    self.last_error_log = now
                    self.log(f"[!] Ошибка скролла тачем: {err}")

    def poll(self):
        """Обрабатывает события окна. False, если окно закрыли"""
        for e in pygame.event.get():
            if e.type == pygame.QUIT:
                return False
            try:
                self._handle(e)
            except SdkError as err:
                now = time.time()
                if now - self.last_error_log > 2:
                    self.last_error_log = now
                    self.log(f"[!] Ошибка отправки ввода: {err}")
        self._flush_wheel()
        # Принудительно держим нужное состояние курсора каждую итерацию:
        # если что-то вернуло курсор ОС (фокус, тулбар, WM), снова прячем.
        self._apply_cursor()
        return True

    def close(self):
        pygame.mouse.set_visible(True)
        pygame.quit()
