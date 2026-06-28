#!/usr/bin/env sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required to install agent-handoff" >&2
    exit 1
fi

cargo install --path "$repo_dir" --locked --force

if [ -n "${CARGO_INSTALL_ROOT:-}" ]; then
    bin_dir="${CARGO_INSTALL_ROOT%/}/bin"
else
    cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    bin_dir="${cargo_home%/}/bin"
fi

case ":$PATH:" in
    *":$bin_dir:"*) added=0 ;;
    *) added=1 ;;
esac

if [ "$added" -eq 1 ]; then
    shell_name=$(basename "${SHELL:-sh}")
    case "$shell_name" in
        zsh) profile="$HOME/.zshrc" ;;
        bash) profile="$HOME/.bashrc" ;;
        *) profile="$HOME/.profile" ;;
    esac

    mkdir -p "$(dirname "$profile")"
    touch "$profile"
    marker="# agent-handoff: cargo bin on PATH"
    if ! grep -F "$marker" "$profile" >/dev/null 2>&1; then
        {
            printf '\n%s\n' "$marker"
            printf 'case ":$PATH:" in\n'
            printf '    *":%s:"*) ;;\n' "$bin_dir"
            printf '    *) export PATH="%s:$PATH" ;;\n' "$bin_dir"
            printf 'esac\n'
        } >>"$profile"
        echo "Added $bin_dir to PATH in $profile"
    fi
fi

if [ -x "$bin_dir/agent-handoff" ]; then
    resolved="$bin_dir/agent-handoff"
elif command -v agent-handoff >/dev/null 2>&1; then
    resolved=$(command -v agent-handoff)
else
    resolved="$bin_dir/agent-handoff"
fi

"$resolved" --version
echo "agent-handoff installed at $resolved"
