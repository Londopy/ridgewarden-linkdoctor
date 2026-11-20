#!/bin/sh
# Install ridgewarden-linkdoctor on a Linux box that already runs the ridgewarden core.
#
#   curl -fsSL https://github.com/Londopy/ridgewarden-linkdoctor/releases/latest/download/install.sh | sudo sh
#
# Optional: RIDGEWARDEN_LINKDOCTOR_VERSION=0.2.0 to pin a version.
set -eu

REPO=Londopy/ridgewarden-linkdoctor
BIN=/usr/local/bin/ridgewarden-linkdoctor

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root (with sudo)." >&2
    exit 1
fi

# the core checks this release's signature, and the app needs the core to run
core=$(command -v ridgewarden || true)
if [ -z "$core" ]; then
    echo "Install the ridgewarden core first: https://github.com/Londopy/ridgewarden" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *)
        echo "No build for $(uname -m). 64-bit Raspberry Pi OS and x86_64 Linux are supported." >&2
        exit 1
        ;;
esac

pinned=${RIDGEWARDEN_LINKDOCTOR_VERSION:-}
if [ -n "$pinned" ]; then
    tag="v$pinned"
else
    tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
[ -n "$tag" ] || { echo "Could not find the latest release." >&2; exit 1; }
ver=${tag#v}
file="ridgewarden-linkdoctor-v$ver-$arch-linux.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ridgewarden-linkdoctor $tag for $arch"
for f in "$file" SHA256SUMS.txt SHA256SUMS.txt.sig; do
    curl -fsSL -o "$tmp/$f" "$base/$f"
done

# the installed core checks the signature against the release key it already trusts
(cd "$tmp" && grep " $file\$" SHA256SUMS.txt | sha256sum -c -)
"$core" verify-release "$tmp/SHA256SUMS.txt"
tar -xzf "$tmp/$file" -C "$tmp"
dir="$tmp/ridgewarden-linkdoctor-v$ver-$arch-linux"

install -m 755 "$dir/ridgewarden-linkdoctor" "$BIN"
if ! id ridgewarden-linkdoctor >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/ridgewarden-linkdoctor --shell /usr/sbin/nologin --user-group ridgewarden-linkdoctor
fi
usermod -a -G ridgewarden ridgewarden-linkdoctor
install -d -m 750 -o ridgewarden-linkdoctor -g ridgewarden-linkdoctor /var/lib/ridgewarden-linkdoctor
# an upgrade from when apps ran as the core's user leaves files it owned
chown -R ridgewarden-linkdoctor:ridgewarden-linkdoctor /var/lib/ridgewarden-linkdoctor
if [ ! -f /etc/ridgewarden/linkdoctor.toml ]; then
    "$BIN" --config /etc/ridgewarden/linkdoctor.toml init
fi
# its webhook URLs and passphrases are for this app only, not the other apps
# in the ridgewarden group
chown root:ridgewarden-linkdoctor /etc/ridgewarden/linkdoctor.toml
chmod 640 /etc/ridgewarden/linkdoctor.toml

if [ -d /run/systemd/system ]; then
    sed "s#/usr/bin/ridgewarden-linkdoctor#$BIN#" "$dir/ridgewarden-linkdoctor.service" >/etc/systemd/system/ridgewarden-linkdoctor.service
    systemctl daemon-reload
    systemctl enable ridgewarden-linkdoctor
    if [ -f /etc/ridgewarden/ridgewarden.toml ]; then
        # restart, so an upgrade replaces a running old version
        systemctl restart ridgewarden-linkdoctor
    else
        echo "Set up the core first (ridgewarden init), then: sudo systemctl start ridgewarden-linkdoctor"
        exit 0
    fi
    echo "ridgewarden-linkdoctor is running: http://$(hostname):8792/"
else
    echo "Installed $BIN ($tag). Start it with: $BIN --config /etc/ridgewarden/linkdoctor.toml run"
fi
