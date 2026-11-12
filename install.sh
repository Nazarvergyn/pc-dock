#!/bin/sh
# Установщик PC Dock (Linux, macOS): скачивает релиз, проверяет SHA256SUMS, ставит бинарники в ~/.pcdock/bin.
#   curl -fsSL https://raw.githubusercontent.com/toxic-sunflower/pc-dock/main/install.sh | sh
#   sh install.sh [версия]        например v0.1.0-alpha.2; без версии берётся самый свежий релиз (в том числе альфа)
# Переменные: PCDOCK_HOME (по умолчанию ~/.pcdock), PCDOCK_VERSION.
set -eu

REPO="toxic-sunflower/pc-dock"
HOME_DIR="${PCDOCK_HOME:-$HOME/.pcdock}"
VERSION="${1:-${PCDOCK_VERSION:-}}"

say() { printf '%s\n' "$*"; }
die() { printf 'ошибка: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "нужна программа $1"; }

need curl
need tar

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) PLATFORM="linux-x86_64" ;;
  Darwin-arm64) PLATFORM="macos-arm64" ;;
  *) die "эта система не поддерживается: $(uname -s) $(uname -m)" ;;
esac

if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=1" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$VERSION" ] || die "не удалось узнать последний релиз"
fi

NAME="pcdock-$VERSION-$PLATFORM"
BASE="https://github.com/$REPO/releases/download/$VERSION"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "Скачиваю $NAME"
curl -fsSL -o "$TMP/$NAME.tar.gz" "$BASE/$NAME.tar.gz" || die "нет такого релиза: $VERSION"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" || die "нет файла SHA256SUMS"

want=$(grep " $NAME.tar.gz\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
[ -n "$want" ] || die "в SHA256SUMS нет $NAME.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then have=$(sha256sum "$TMP/$NAME.tar.gz" | cut -d' ' -f1)
else have=$(shasum -a 256 "$TMP/$NAME.tar.gz" | cut -d' ' -f1); fi
[ "$want" = "$have" ] || die "контрольная сумма не совпала"

tar xzf "$TMP/$NAME.tar.gz" -C "$TMP"
mkdir -p "$HOME_DIR/bin" "$HOME_DIR/docs"
for b in pcdockd pcdock-autod pcdock-check pcdock-probe; do
  install -m 755 "$TMP/$NAME/$b" "$HOME_DIR/bin/$b"
done
rm -rf "$HOME_DIR/docs" && cp -R "$TMP/$NAME/docs" "$HOME_DIR/docs"

say "Установлено в $HOME_DIR/bin (версия $VERSION)"
case ":$PATH:" in
  *":$HOME_DIR/bin:"*) ;;
  *) say "Добавьте в PATH (например, в ~/.profile):"
     say "  export PATH=\"$HOME_DIR/bin:\$PATH\"" ;;
esac
say "Проверка телефонов: pcdock-check. Документация: $HOME_DIR/docs/README.md"
