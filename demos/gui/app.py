"""Демо-GUI PC Dock: окно с плитками телефонов поверх протокола демона `pcdockd` (TCP или локальная труба).

    uv run app.py                       # tcp://127.0.0.1:9001
    uv run app.py --addr local:pcdock   # локальная труба
    uv run app.py --fps 15 --token ...

Мышь по плитке: касание (нажатие и протяжка), колесо: прокрутка. Клавиатура идёт на выбранный телефон
(Tab: следующий телефон). Пробел с зажатым Ctrl: разбудить. Заголовок плитки показывает состояние телефона
и подсказку, если нужны действия на телефоне.
"""
import argparse
import io
import sys

import pygame

from client import Daemon, RemoteError

# Коды USB HID (страница Keyboard) для клавиш pygame.
KEYMAP = {pygame.K_RETURN: 0x28, pygame.K_ESCAPE: 0x29, pygame.K_BACKSPACE: 0x2A, pygame.K_SPACE: 0x2C,
          pygame.K_RIGHT: 0x4F, pygame.K_LEFT: 0x50, pygame.K_DOWN: 0x51, pygame.K_UP: 0x52}
for i, c in enumerate("abcdefghijklmnopqrstuvwxyz"):
    KEYMAP[getattr(pygame, f"K_{c}")] = 0x04 + i
for i in range(1, 10):
    KEYMAP[getattr(pygame, f"K_{i}")] = 0x1E + i - 1
KEYMAP[pygame.K_0] = 0x27

COLORS = {"ready": (60, 160, 60), "connecting": (120, 120, 120), "appeared": (120, 120, 120), "lost": (90, 90, 90)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--addr", default="tcp://127.0.0.1:9001")
    ap.add_argument("--token")
    ap.add_argument("--fps", type=float, default=15, help="сколько кадров в секунду просить у телефонов")
    args = ap.parse_args()

    try:
        daemon = Daemon(args.addr, args.token)
    except (OSError, RemoteError) as e:
        sys.exit(f"не удалось подключиться к {args.addr}: {e}")

    phones = {}          # номер -> сведения
    watched = set()

    def refresh():
        for d in daemon.request("List").get("details", []):
            phones[d["phone"]] = d

    def on_event(v):
        p = v.get("phone")
        if v["ev"] == "Appeared":
            phones.setdefault(p, {"phone": p}).update(transport=v["transport"], serial=v["serial"])
        elif v["ev"] == "State":
            phones.setdefault(p, {"phone": p}).update(state=v["state"], hint=v["detail"])
        elif v["ev"] == "Lost":
            phones.pop(p, None)
            watched.discard(p)

    daemon.on_event = on_event
    refresh()

    pygame.init()
    screen = pygame.display.set_mode((1100, 760), pygame.RESIZABLE)
    pygame.display.set_caption("PC Dock: демо")
    font = pygame.font.Font(None, 22)
    selected = None
    textures = {}        # номер -> (номер кадра, Surface)
    dragging = None      # (телефон, прямоугольник плитки)
    clock = pygame.time.Clock()

    def tiles():
        ids = sorted(phones)
        n = max(1, len(ids))
        cols = min(n, 4)
        rows = (n + cols - 1) // cols
        w, h = screen.get_width() // cols, screen.get_height() // rows
        return {pid: pygame.Rect((i % cols) * w, (i // cols) * h, w, h) for i, pid in enumerate(ids)}

    running = True
    while running:
        layout = tiles()
        for pid, d in phones.items():
            if d.get("state") == "ready" and pid not in watched:
                try:
                    daemon.request("Watch", phone=pid, fps=args.fps)
                    watched.add(pid)
                except RemoteError:
                    pass
        for ev in pygame.event.get():
            if ev.type == pygame.QUIT:
                running = False
            elif ev.type == pygame.KEYDOWN and ev.key == pygame.K_TAB:
                ids = sorted(phones)
                if ids:
                    selected = ids[(ids.index(selected) + 1) % len(ids)] if selected in ids else ids[0]
            elif ev.type in (pygame.KEYDOWN, pygame.KEYUP) and selected is not None and ev.key in KEYMAP:
                daemon.send("Key", phone=selected, usage=KEYMAP[ev.key], down=ev.type == pygame.KEYDOWN)
            elif ev.type == pygame.KEYDOWN and ev.key == pygame.K_SPACE and ev.mod & pygame.KMOD_CTRL and selected:
                daemon.send("Wake", phone=selected)
            elif ev.type == pygame.MOUSEBUTTONDOWN and ev.button == 1:
                for pid, r in layout.items():
                    if r.collidepoint(ev.pos):
                        selected, dragging = pid, (pid, r)
                        x, y = rel(ev.pos, r, textures.get(pid))
                        daemon.send("TouchEx", phone=pid, cid=0, phase=0, x=x, y=y, width=None, height=None, pressure=0.7)
            elif ev.type == pygame.MOUSEMOTION and dragging and ev.buttons[0]:
                x, y = rel(ev.pos, dragging[1], textures.get(dragging[0]))
                daemon.send("TouchEx", phone=dragging[0], cid=0, phase=1, x=x, y=y, width=None, height=None, pressure=0.7)
            elif ev.type == pygame.MOUSEBUTTONUP and ev.button == 1 and dragging:
                x, y = rel(ev.pos, dragging[1], textures.get(dragging[0]))
                daemon.send("TouchEx", phone=dragging[0], cid=0, phase=2, x=x, y=y, width=None, height=None, pressure=0.7)
                dragging = None
            elif ev.type == pygame.MOUSEWHEEL and selected is not None:
                daemon.send("Scroll", phone=selected, ticks=float(ev.y))

        screen.fill((20, 20, 20))
        for pid, r in layout.items():
            frame = daemon.frames.get(pid)
            if frame and (pid not in textures or textures[pid][0] != frame[0]):
                try:
                    textures[pid] = (frame[0], pygame.image.load(io.BytesIO(frame[1]), "x.jpg").convert())
                except pygame.error:
                    pass
            tex = textures.get(pid)
            body = r.inflate(-8, -34)
            body.top = r.top + 30
            if tex:
                s = tex[1]
                k = min(body.width / s.get_width(), body.height / s.get_height())
                scaled = pygame.transform.smoothscale(s, (int(s.get_width() * k), int(s.get_height() * k)))
                screen.blit(scaled, scaled.get_rect(center=body.center))
            d = phones.get(pid, {})
            state = d.get("state", "?")
            pygame.draw.rect(screen, (90, 160, 255) if pid == selected else (60, 60, 60), r.inflate(-2, -2), 2)
            label = f"{pid}  {state}  {d.get('transport') or ''} {d.get('serial') or ''}"
            screen.blit(font.render(label, True, COLORS.get(state, (230, 150, 40))), (r.left + 8, r.top + 6))
            if d.get("hint"):
                hint = font.render(d["hint"][:80], True, (230, 150, 40))
                screen.blit(hint, (r.left + 8, r.bottom - 22))
        if not phones:
            screen.blit(font.render("Телефонов нет. Подключите телефон: ферма найдёт его сама.", True, (200, 200, 200)), (20, 20))
        pygame.display.flip()
        clock.tick(60)
    daemon.close()


def rel(pos, rect, tex):
    """Доли 0..1 в картинке, вписанной в плитку (если кадра ещё нет, от плитки)."""
    body = rect.inflate(-8, -34)
    body.top = rect.top + 30
    if tex:
        s = tex[1]
        k = min(body.width / s.get_width(), body.height / s.get_height())
        w, h = s.get_width() * k, s.get_height() * k
        left, top = body.centerx - w / 2, body.centery - h / 2
    else:
        w, h, left, top = body.width, body.height, body.left, body.top
    return min(1.0, max(0.0, (pos[0] - left) / w)), min(1.0, max(0.0, (pos[1] - top) / h))


if __name__ == "__main__":
    main()
