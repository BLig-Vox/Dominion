#!/bin/sh
set -eu

fail() {
    printf 'Dominion: %s\n' "$*" >&2
    exit 1
}

desktop=no
update=no
register_only=no
channel=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --desktop) desktop=yes ;;
        --update) update=yes ;;
        --edge|--stable) channel=${1#--} ;;
        --register-only) register_only=yes ;;
        --help|-h)
            printf '%s\n' 'Usage: sh install.sh [--desktop] [--update] [--edge | --stable]'
            exit 0
            ;;
        *) fail "Unknown option $1. Run sh install.sh --help." ;;
    esac
    shift
done

[ "$(uname -s)" = Linux ] || fail 'The installer supports Linux.'
binary=${DOMINION_EXECUTABLE:-"$HOME/.local/bin/dominion"}
case "$binary" in
    /*) ;;
    *) fail 'The installation path must be absolute.' ;;
esac
bin_dir=$(dirname -- "$binary")
channel_file="$binary.channel"

register_desktop() {
    data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
    apps="$data_home/applications"
    icons="$data_home/icons/hicolor/128x128/apps"
    mkdir -p -- "$apps" "$icons"
    "$binary" --print-icon > "$icons/dominion.png"
    # The desktop entry parser and the Exec parser each consume escapes.
    exec_path=$(printf '%s' "$binary" | sed -e 's/[\\]/&&&&/g' -e 's/["`$]/\\\\&/g' -e 's/%/%%/g')
    cat > "$apps/dominion.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Dominion
Comment=Monitor processes and inspect disk usage
Exec="$exec_path"
Icon=dominion
Terminal=false
Categories=System;Monitor;
DESKTOP
}

if [ "$register_only" = yes ]; then
    [ "$desktop" = yes ] && [ "$update" = no ] && [ -z "$channel" ] ||
        fail 'Desktop registration cannot be combined with release options.'
    [ -x "$binary" ] || fail 'Install Dominion before adding a desktop launcher.'
    register_desktop
    printf '%s\n' 'Added Dominion to the app menu.'
    exit 0
fi

case "$(uname -m)" in
    x86_64) target=x86_64-unknown-linux-gnu ;;
    *) fail 'The release binary supports Linux x86-64 with glibc.' ;;
esac
if [ "$update" = yes ]; then
    [ -x "$binary" ] || fail 'Dominion is not installed. Run the installer without --update.'
fi

if [ -z "$channel" ]; then
    channel=stable
    if [ -f "$channel_file" ]; then
        IFS= read -r channel < "$channel_file"
    fi
fi
case "$channel" in
    stable) set -- ;;
    edge) set -- edge ;;
    *) fail 'Unknown saved channel. Choose --stable or --edge.' ;;
esac

command -v gh >/dev/null 2>&1 || fail 'Install GitHub CLI and sign in with gh auth login.'
archive="dominion-$target.tar.gz"
mkdir -p -- "$bin_dir"
stage=$(mktemp -d "$bin_dir/.dominion.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT
trap 'exit 1' HUP INT TERM
gh release download "$@" --repo BLig-Vox/Dominion --dir "$stage" \
    --pattern "$archive" --pattern "$archive.sha256" ||
    fail 'Could not download the release. Check your GitHub CLI login and repository access.'
(
    cd "$stage"
    sha256sum --check --strict "$archive.sha256"
    tar -xOf "$archive" dominion > dominion
    chmod 755 dominion
    ./dominion --version
)
printf '%s\n' "$channel" > "$stage/channel"
mv -fT -- "$stage/dominion" "$binary"
mv -fT -- "$stage/channel" "$channel_file"

if [ "$desktop" = yes ]; then
    register_desktop
else
    data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
    if [ -f "$data_home/applications/dominion.desktop" ]; then
        old_icon="$data_home/icons/hicolor/128x128/apps/dominion.png"
        if [ -f "$old_icon" ]; then
            "$binary" --print-icon > "$old_icon"
        fi
    fi
fi
printf 'Installed Dominion (%s) at %s\n' "$channel" "$binary"
