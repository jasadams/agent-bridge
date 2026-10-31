#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Download a checked prebuilt executable; the binary owns service upgrade/rollback.
set -eu
umask 077

fail() { printf 'agent-bridge installer: %s\n' "$*" >&2; exit 1; }
usage() {
    cat <<'EOF'
Usage: install.sh [--version TAG] [--no-service] [--bin-dir ABSOLUTE_PATH]
                  [--state-dir ABSOLUTE_PATH]

Downloads a SHA-256 checked Linux/macOS executable from jasadams/agent-bridge.
The installer selects the latest published semantic version by default.
Installs and starts the per-user service unless --no-service is supplied.
A custom --bin-dir requires --no-service. No sudo, Rust toolchain or Node.js needed.
Binary-only updates refuse the default path when a managed service is configured.
Hooks, native permissions, pairings and peer networking are not automatically changed.
EOF
}
need_value() { [ "$#" -ge 2 ] && [ -n "$2" ] || fail "$1 requires a value"; }
absolute_path() {
    case "$2" in /*) ;; *) fail "$1 must be an absolute path" ;; esac
    case "$2" in */../*|*/..|*/./*|*/.) fail "$1 must not contain dot-directory components" ;; esac
    case "$2" in *'
'*|*"$(printf '\r')"*) fail "$1 must not contain newline characters" ;; esac
}

version=latest
no_service=false
bin_dir=
state_dir=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) need_value "$@"; version=$2; shift 2 ;;
        --no-service) no_service=true; shift ;;
        --bin-dir) need_value "$@"; bin_dir=$2; shift 2 ;;
        --state-dir) need_value "$@"; state_dir=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; [ "$#" -eq 0 ] || fail 'unexpected positional arguments' ;;
        *) fail "unknown option: $1 (use --help)" ;;
    esac
done
case "$version" in ''|[-.]*|*[!A-Za-z0-9._-]*) fail 'invalid release tag' ;; esac
[ "${#version}" -le 128 ] || fail 'release tag is too long'
semantic_tag() {
    printf '%s\n' "$1" | awk '
        /^v[0-9]+\.[0-9]+\.[0-9]+$/ {
            sub(/^v/, ""); split($0, parts, ".")
            for (i=1; i<=3; i++) if (length(parts[i])>1 && substr(parts[i],1,1)=="0") exit 1
            valid=1
        }
        END { if (!valid || NR != 1) exit 1 }
    '
}
if [ "$version" != latest ]; then
    semantic_tag "$version" || fail 'version must be a semantic release tag such as v0.2.0'
fi
[ -n "${HOME:-}" ] || fail 'HOME must be set'
absolute_path HOME "$HOME"
default_bin_dir=$HOME/.local/bin
[ -n "$bin_dir" ] || bin_dir=$default_bin_dir
absolute_path --bin-dir "$bin_dir"
while [ "$bin_dir" != / ] && [ "${bin_dir%/}" != "$bin_dir" ]; do bin_dir=${bin_dir%/}; done
[ -z "$state_dir" ] || absolute_path --state-dir "$state_dir"
if [ "$no_service" = false ] && [ "$bin_dir" != "$default_bin_dir" ]; then
    fail 'a custom --bin-dir requires --no-service; managed services use ~/.local/bin'
fi
for tool in uname id curl mktemp awk cp chmod mv rm mkdir stat; do
    command -v "$tool" >/dev/null 2>&1 || fail "required command is missing: $tool"
done
installer_uid=$(id -u)
[ "$installer_uid" != 0 ] || fail 'run as your normal user, without sudo'
case "$(uname -s)" in
    Darwin) platform=macos ;;
    Linux)
        platform=linux
        libc_banner=$(ldd --version 2>&1 || :)
        case "$libc_banner" in *musl*|*Musl*) fail 'musl Linux is not supported by these GNU/Linux binaries; build from source instead' ;; esac
        libc_version=$(getconf GNU_LIBC_VERSION 2>/dev/null || :)
        case "$libc_version" in glibc\ *) ;; *) fail 'cannot identify GNU libc; build from source on this Linux system' ;; esac
        printf '%s\n' "$libc_version" | awk '
            NF == 2 && $1 == "glibc" {
                split($2, version, ".")
                if (version[1] ~ /^[0-9]+$/ && version[2] ~ /^[0-9]+$/)
                    ok=(version[1] > 2 || (version[1] == 2 && version[2] >= 35))
            }
            END { exit !ok }
        ' || fail 'these Linux binaries require glibc 2.35 or later; build from source on older systems' 
        ;;
    *) fail 'supported platforms are Linux (GNU libc) and macOS' ;;
esac
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail 'supported architectures are x86_64 and arm64' ;;
esac
if [ "$platform" = macos ] && [ "$arch" = x86_64 ]; then
    fail 'Intel macOS is not supported by current releases; use the installer from an older release tag or build from source'
fi
if command -v sha256sum >/dev/null 2>&1; then
    checksum_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum_tool=shasum
else
    fail 'sha256sum or shasum is required to verify the download'
fi

# Resolve the repair boundary physically: a symlink below HOME must never let us
# change permissions on a directory elsewhere on the machine.
physical_home=$(CDPATH= cd "$HOME" && pwd -P) || fail 'cannot resolve HOME'
# Protect private staging and destination paths against replacement by another
# user through a writable/foreign-owned ancestor, including symlink ancestors.
check_directory_chain() (
    directory=$1
    # Secure parents before descendants so a writable parent cannot redirect a
    # later chmod. The subshell keeps recursive calls' variables independent.
    if [ "$directory" != / ]; then
        parent_directory=${directory%/*}
        [ -n "$parent_directory" ] || parent_directory=/
        check_directory_chain "$parent_directory" || exit 1
    fi
    if [ "$platform" = macos ]; then
        metadata=$(stat -f '%u %Lp' "$directory") || fail 'cannot inspect an installation directory'
    else
        metadata=$(stat -c '%u %a' "$directory") || fail 'cannot inspect an installation directory'
    fi
    directory_owner=${metadata%% *}
    directory_mode=${metadata#* }
    case "$directory_owner" in ''|*[!0-9]*) fail 'cannot identify directory owner' ;; esac
    case "$directory_mode" in ''|*[!0-7]*) fail 'cannot identify directory permissions' ;; esac
    [ "$directory_owner" = "$installer_uid" ] || [ "$directory_owner" = 0 ] || fail 'installation directories must belong to your user or root'
    if [ ! -L "$directory" ] && [ "$((0$directory_mode & 0022))" -ne 0 ]; then
        if [ "$directory_owner" = 0 ] && [ "$((0$directory_mode & 01000))" -ne 0 ]; then
            : # Shared root-owned sticky temporary directory.
        else
            physical_checked=$(CDPATH= cd "$directory" && pwd -P) || fail "cannot resolve installation directory: $directory"
            case "$physical_checked" in
                "$physical_home"|"$physical_home"/*)
                    [ "$directory_owner" = "$installer_uid" ] || fail "installation directory is writable by others and is not owned by you: $directory"
                    physical_parent_to_check=${physical_checked%/*}
                    [ -n "$physical_parent_to_check" ] || physical_parent_to_check=/
                    check_directory_chain "$physical_parent_to_check" || exit 1
                    printf 'Securing installation directory: %s (removing group/other write access)\n' "$directory" >&2
                    chmod go-w "$physical_checked" || fail "cannot secure installation directory: $directory"
                    if [ "$platform" = macos ]; then
                        repaired_metadata=$(stat -f '%u %Lp' "$physical_checked")
                    else
                        repaired_metadata=$(stat -c '%u %a' "$physical_checked")
                    fi
                    repaired_owner=${repaired_metadata%% *}
                    repaired_mode=${repaired_metadata#* }
                    [ "$repaired_owner" = "$installer_uid" ] && [ "$((0$repaired_mode & 0022))" -eq 0 ] || fail "installation directory is still writable by others: $directory"
                    ;;
                *) fail "installation directory is writable by others outside your home: $directory" ;;
            esac
        fi
    fi
)
safe_directory() {
    [ -d "$1" ] || fail 'installation directory does not exist'
    check_directory_chain "$1"
    physical_directory=$(CDPATH= cd "$1" && pwd -P) || fail 'cannot resolve installation directory'
    check_directory_chain "$physical_directory"
    printf '%s\n' "$physical_directory"
}
check_existing_ancestor() {
    ancestor=$1
    while [ ! -d "$ancestor" ]; do
        [ ! -e "$ancestor" ] && [ ! -L "$ancestor" ] || fail 'an installation parent is not a directory'
        ancestor=${ancestor%/*}
        [ -n "$ancestor" ] || ancestor=/
    done
    safe_directory "$ancestor" >/dev/null
}
secure_create_directory() {
    check_existing_ancestor "$1"
    mkdir -p "$1"
    safe_directory "$1"
}
# Older/manual installs may have used the user's default group-writable umask.
# Normalize only the two files this managed upgrade will replace or preserve.
secure_existing_install_file() {
    install_file=$1
    [ -e "$install_file" ] || [ -L "$install_file" ] || return 0
    [ -f "$install_file" ] && [ ! -L "$install_file" ] || fail "installation file is not a regular non-symlink file: $install_file"
    install_parent=${install_file%/*}
    physical_parent=$(safe_directory "$install_parent")
    case "$physical_parent" in
        "$physical_home"|"$physical_home"/*) ;;
        *) fail "managed installation file is outside your home: $install_file" ;;
    esac
    if [ "$platform" = macos ]; then
        file_metadata=$(stat -f '%u %Lp %l' "$install_file")
    else
        file_metadata=$(stat -c '%u %a %h' "$install_file")
    fi
    [ "${file_metadata%% *}" = "$installer_uid" ] || fail "installation file is not owned by you: $install_file"
    file_details=${file_metadata#* }
    file_mode=${file_details%% *}
    file_links=${file_details#* }
    [ "$file_links" = 1 ] || fail "refusing to change a hard-linked installation file: $install_file"
    if [ "$((0$file_mode & 0022))" -ne 0 ]; then
        printf 'Securing installation file: %s (removing group/other write access)\n' "$install_file" >&2
        chmod go-w "$install_file" || fail "cannot secure installation file: $install_file"
    fi
}
check_binary_destination() {
    destination=$bin_dir/agent-bridge
    [ ! -L "$destination" ] || fail 'refusing to replace a symbolic-link executable'
    if [ -e "$destination" ] && [ ! -f "$destination" ]; then fail 'destination is not a regular file'; fi
    if [ "$bin_dir" = "$default_bin_dir" ]; then
        if [ "$platform" = linux ]; then
            manager_file=$HOME/.config/systemd/user/agent-bridge.service
        else
            manager_file=$HOME/Library/LaunchAgents/local.agent-bridge.plist
        fi
        if [ -e "$manager_file" ] || [ -L "$manager_file" ]; then
            fail 'a managed service is configured; omit --no-service for a backed-up service upgrade, or choose a separate --bin-dir'
        fi
        if [ "$platform" = linux ] && command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet agent-bridge.service; then
            fail 'the managed service is running; omit --no-service for a backed-up service upgrade'
        fi
        if [ "$platform" = macos ] && command -v launchctl >/dev/null 2>&1 && launchctl print "gui/$(id -u)/local.agent-bridge" >/dev/null 2>&1; then
            fail 'the managed service is running; omit --no-service for a backed-up service upgrade'
        fi
    fi
}
[ "$no_service" = false ] || check_binary_destination
# Do these checks before downloading or executing an asset.
absolute_path TMPDIR "${TMPDIR:-/tmp}"
check_existing_ancestor "$bin_dir"
if [ "$no_service" = false ]; then
    if [ "$platform" = linux ]; then
        check_existing_ancestor "$HOME/.config/systemd/user"
    else
        check_existing_ancestor "$HOME/Library/LaunchAgents"
    fi
    check_existing_ancestor "$HOME/.local/state/agent-bridge-backups"
    check_existing_ancestor "${state_dir:-$HOME/Library/Application Support/AgentBridge}"
fi
private_tmp_parent=$(safe_directory "${TMPDIR:-/tmp}")

work_dir=
staged=
staged_notices=
cleanup() {
    [ -z "$staged" ] || rm -f "$staged"
    [ -z "$staged_notices" ] || rm -rf "$staged_notices"
    [ -z "$work_dir" ] || rm -rf "$work_dir"
}
trap cleanup 0
trap 'exit 1' 1 2 3 15
work_dir=$(mktemp -d "$private_tmp_parent/agent-bridge-install.XXXXXXXX") || fail 'cannot create a private download directory'
asset=agent-bridge-$platform-$arch
if [ "$version" = latest ]; then
    release_url=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
        --silent --show-error --retry 3 --connect-timeout 10 --max-time 120 \
        --output /dev/null --write-out '%{url_effective}' \
        https://github.com/jasadams/agent-bridge/releases/latest) || fail 'cannot resolve the latest published release'
    case "$release_url" in
        https://github.com/jasadams/agent-bridge/releases/tag/*) version=${release_url##*/} ;;
        *) fail 'latest release resolved to an unexpected URL' ;;
    esac
    [ "$release_url" = "https://github.com/jasadams/agent-bridge/releases/tag/$version" ] || fail 'latest release resolved to an unexpected URL'
    [ "${#version}" -le 128 ] && semantic_tag "$version" || fail 'latest release is not a semantic version'
fi
base_url=https://github.com/jasadams/agent-bridge/releases/download/$version
download() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
        --silent --show-error --retry 3 --connect-timeout 10 --max-time 300 \
        --output "$2" "$1" || fail "download failed; check that release '$version' has published $asset and SHA256SUMS"
}
printf 'Downloading Agent Bridge %s for %s/%s...\n' "$version" "$platform" "$arch"
download "$base_url/SHA256SUMS" "$work_dir/SHA256SUMS"
for filename in "$asset" LICENSE THIRD_PARTY_LICENSES.txt SOURCE_COMMIT SOURCE_VERSION; do
    download "$base_url/$filename" "$work_dir/$filename"
    expected=$(awk -v name="$filename" '
        ($2 == name || $2 == "*" name) {
            count++
            if (NF != 2 || length($1) != 64 || $1 ~ /[^a-fA-F0-9]/) bad=1
            digest=tolower($1)
        }
        END { if (count != 1 || bad) exit 1; print digest }
    ' "$work_dir/SHA256SUMS") || fail 'checksum manifest must contain exactly one valid entry for every required asset'
    if [ "$checksum_tool" = sha256sum ]; then
        actual=$(sha256sum "$work_dir/$filename" | awk '{print $1}')
    else
        actual=$(shasum -a 256 "$work_dir/$filename" | awk '{print $1}')
    fi
    [ "$actual" = "$expected" ] || fail 'checksum verification failed; nothing was installed'
done
source_commit=$(awk 'NR==1 && length($0)==40 && $0 !~ /[^a-fA-F0-9]/ {commit=tolower($0);valid=1} END {if(NR!=1 || !valid)exit 1;print commit}' "$work_dir/SOURCE_COMMIT") || fail 'SOURCE_COMMIT must contain one full source commit hash'
source_version=$(awk 'NR==1 {value=$0} END {if(NR!=1)exit 1;print value}' "$work_dir/SOURCE_VERSION") || fail 'SOURCE_VERSION must contain one semantic version'
[ "$source_version" = "${version#v}" ] || fail 'release tag and SOURCE_VERSION disagree; nothing was installed'
chmod 700 "$work_dir/$asset"
binary_version=$("$work_dir/$asset" --version </dev/null) || fail 'the verified executable cannot run on this system; nothing was installed'
[ "$binary_version" = "agent-bridge $source_version" ] || fail 'the executable version disagrees with its release tag; nothing was installed'
printf '%s\n' "$binary_version"

# Persist complete notices before a service upgrade, retaining earlier versions.
if [ "$bin_dir" = "$default_bin_dir" ]; then
    notice_parent=$HOME/.local/share/agent-bridge/licenses
else
    notice_parent=$bin_dir/.agent-bridge-licenses
fi
notice_parent=$(secure_create_directory "$notice_parent")
notice_directory=$notice_parent/$source_commit-$source_version
if [ -e "$notice_directory" ] || [ -L "$notice_directory" ]; then
    [ -d "$notice_directory" ] && [ ! -L "$notice_directory" ] || fail 'existing license destination is unsafe'
    safe_directory "$notice_directory" >/dev/null
    for filename in LICENSE THIRD_PARTY_LICENSES.txt SOURCE_COMMIT SOURCE_VERSION; do
        [ -f "$notice_directory/$filename" ] && [ ! -L "$notice_directory/$filename" ] || fail 'existing license files are unsafe'
        if [ "$checksum_tool" = sha256sum ]; then
            existing=$(sha256sum "$notice_directory/$filename" | awk '{print $1}')
            incoming=$(sha256sum "$work_dir/$filename" | awk '{print $1}')
        else
            existing=$(shasum -a 256 "$notice_directory/$filename" | awk '{print $1}')
            incoming=$(shasum -a 256 "$work_dir/$filename" | awk '{print $1}')
        fi
        [ "$existing" = "$incoming" ] || fail 'existing notices disagree with the source commit; refusing to overwrite them'
    done
else
    staged_notices=$(mktemp -d "$notice_parent/.agent-bridge-notices.XXXXXXXX")
    for filename in LICENSE THIRD_PARTY_LICENSES.txt SOURCE_COMMIT SOURCE_VERSION; do
        cp "$work_dir/$filename" "$staged_notices/$filename"
        chmod 644 "$staged_notices/$filename"
    done
    mv "$staged_notices" "$notice_directory"
    staged_notices=
fi
printf 'License and dependency notices: %s\n' "$notice_directory"

if [ "$no_service" = false ]; then
    secure_existing_install_file "$bin_dir/agent-bridge"
    if [ "$platform" = linux ]; then
        secure_existing_install_file "$HOME/.config/systemd/user/agent-bridge.service"
    else
        secure_existing_install_file "$HOME/Library/LaunchAgents/local.agent-bridge.plist"
    fi
    # Service code stops/backups the old managed service and performs a recoverable
    # binary/state upgrade. Do not replace the installed executable beforehand.
    if [ -n "$state_dir" ]; then
        "$work_dir/$asset" --state-dir "$state_dir" service install </dev/null
    else
        "$work_dir/$asset" service install </dev/null
    fi
else
    check_binary_destination
    bin_dir=$(secure_create_directory "$bin_dir")
    destination=$bin_dir/agent-bridge
    staged=$(mktemp "$bin_dir/.agent-bridge.new.XXXXXXXX") || fail 'cannot stage the executable in the install directory'
    cp "$work_dir/$asset" "$staged"
    chmod 755 "$staged"
    if [ -f "$destination" ]; then
        backup=$(mktemp "$bin_dir/.agent-bridge.backup.XXXXXXXX") || fail 'cannot create an executable backup'
        cp -p "$destination" "$backup"
        printf 'Previous executable retained at %s\n' "$backup"
    fi
    mv -f "$staged" "$destination"
    staged=
    printf 'Installed %s (service not changed).\n' "$destination"
fi
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) printf 'Add %s to your PATH to run agent-bridge from a new terminal.\n' "$bin_dir" ;;
esac
printf 'Done. Inspect status with: %s/agent-bridge status\n' "$bin_dir"
