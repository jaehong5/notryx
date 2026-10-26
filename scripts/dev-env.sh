#!/usr/bin/env bash

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    printf '%s\n' 'Load this file with: source scripts/dev-env.sh' >&2
    exit 1
fi

if [[ -f "$HOME/.cargo/env" ]]; then
    source "$HOME/.cargo/env"
fi

export ANDROID_HOME="${ANDROID_HOME:-$HOME/.local/share/notryx/android-sdk}"
export NDK_HOME="${NDK_HOME:-$ANDROID_HOME/ndk/30.0.16248370}"

if [[ -z ${JAVA_HOME:-} ]] && command -v javac >/dev/null 2>&1; then
    JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$(command -v javac)")")")"
    export JAVA_HOME
fi

_notryx_dev_path_add() {
    case ":$PATH:" in
        *":$1:"*) ;;
        *) export PATH="$1:$PATH" ;;
    esac
}

_notryx_dev_path_add "$ANDROID_HOME/cmdline-tools/latest/bin"
_notryx_dev_path_add "$ANDROID_HOME/platform-tools"
_notryx_dev_path_add "$HOME/.local/share/notryx/tooling/node_modules/.bin"
unset -f _notryx_dev_path_add
