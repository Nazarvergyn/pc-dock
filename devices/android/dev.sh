#!/usr/bin/env bash
# Один скрипт на весь цикл: сборка -> установка (push + pm) -> запуск viewer -> сбор логов.
# Использование:
#   ./dev.sh          полный цикл
#   ./dev.sh install  только собрать и поставить
#   ./dev.sh logs     только снять логи с телефона
set -u
cd "$(dirname "$0")"

APK=app-screencast/app/build/outputs/apk/debug/app-debug.apk
REMOTE=/data/local/tmp/app-debug.apk
LOGDIR=logs
mkdir -p "$LOGDIR"

wait_adb() {
    echo "[dev] Жду телефон по adb (до 120 с)... Подключите его в обычном режиме"
    timeout 120 adb wait-for-device || { echo "[dev] adb не увидел телефон"; return 1; }
    adb devices | sed 's/^/[dev]   /'
}

build() {
    echo "[dev] Сборка..."
    (cd app-screencast && ./gradlew --offline :app:assembleDebug -q 2>&1 | grep -v '^openjdk\|^OpenJDK') || true
    [ -f "$APK" ] || { echo "[dev] APK не собрался"; exit 1; }
    ls -l "$APK" | sed 's/^/[dev] /'
}

install() {
    wait_adb || exit 1
    echo "[dev] push..."
    timeout 90 adb push "$APK" "$REMOTE" || { echo "[dev] push завис/упал"; exit 1; }
    echo "[dev] pm install..."
    timeout 90 adb shell pm install -r -t "$REMOTE" || { echo "[dev] pm install упал"; exit 1; }
    adb shell rm -f "$REMOTE"
    adb logcat -c
    echo "[dev] Установлено, logcat очищен"
    # Переустановка убивает процесс и одноразовый токен проекции: открываем приложение,
    # чтобы пользователь один раз подтвердил запись экрана. Не критично, если не выйдет.
    adb shell am start -n com.device.media/.MainActivity >/dev/null 2>&1 || true
    echo "[dev] Если на телефоне открылся запрос записи экрана — подтвердите его один раз"
}

collect_logs() {
    wait_adb || return 1
    adb logcat -d -s DeviceMedia AndroidRuntime > "$LOGDIR/phone.log"
    echo "[dev] Лог телефона: $LOGDIR/phone.log ($(wc -l < "$LOGDIR/phone.log") строк)"
    echo "---------- последние строки ----------"
    tail -n 40 "$LOGDIR/phone.log"
}

run_viewer() {
    echo "[dev] Запуск viewer. Подключите телефон, подтвердите диалоги, подвигайте экран."
    echo "[dev] Остановка: Ctrl+C (потом переподключите телефон кабелем для снятия логов)"
    (cd viewer && uv run main.py 2>&1 | tee "../$LOGDIR/pc.log") || true
    echo "[dev] Лог ПК: $LOGDIR/pc.log"
}

case "${1:-all}" in
    install) build; install ;;
    logs)    collect_logs ;;
    all)     build; install; run_viewer; collect_logs ;;
    *)       echo "usage: $0 [all|install|logs]"; exit 1 ;;
esac
