#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <version> <deb> <output-directory>" >&2
  exit 2
fi

version="$1"
deb="$(realpath "$2")"
output_dir="$(realpath -m "$3")"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_name="netsanctum-desktop-${version}-1-x86_64.pkg.tar.zst"
work_dir="$(mktemp -d)"
package_root="${work_dir}/root"
mtree="${work_dir}/.MTREE"
data_member=""

trap 'rm -rf "$work_dir"' EXIT

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z]+)*$ ]] || {
  echo "invalid package version: $version" >&2
  exit 2
}
[[ -f "$deb" ]] || {
  echo "Debian package not found: $deb" >&2
  exit 2
}

mkdir -p "$package_root" "$output_dir"
while IFS= read -r member; do
  if [[ "$member" == data.tar.* ]]; then
    data_member="$member"
    break
  fi
done < <(ar t "$deb")
[[ -n "$data_member" ]] || {
  echo "Debian data archive not found in: $deb" >&2
  exit 2
}
ar p "$deb" "$data_member" | bsdtar -xf - -C "$package_root"

test -x "$package_root/usr/bin/netsanctum-desktop"
test -f "$package_root/usr/share/applications/NetSanctum Desktop.desktop"

install -Dm644 "$repo_root/LICENSE" \
  "$package_root/usr/share/licenses/netsanctum-desktop/LICENSE"

installed_size="$(du -sb "$package_root/usr" | cut -f1)"
build_date="${SOURCE_DATE_EPOCH:-$(date +%s)}"

cat >"$package_root/.PKGINFO" <<EOF
pkgname = netsanctum-desktop
pkgbase = netsanctum-desktop
pkgver = ${version}-1
pkgdesc = Secure desktop and offline companion for NetSanctum
url = https://github.com/YatsenkoYura/NetSanctum-Desktop
builddate = ${build_date}
packager = NetSanctum contributors
size = ${installed_size}
arch = x86_64
license = MPL-2.0
depend = glibc
depend = gcc-libs
depend = gtk3
depend = libayatana-appindicator
depend = webkit2gtk-4.1
optdepend = gst-libav: additional media codec support
optdepend = gst-plugins-bad: additional media codec support
optdepend = gst-plugins-good: additional media codec support
EOF

find "$package_root" -exec touch -h -d "@${build_date}" {} +
(
  cd "$package_root"
  export LC_COLLATE=C
  shopt -s dotglob globstar
  printf '%s\0' **/* \
    | bsdtar -cnf - --format=mtree \
      --uid 0 --gid 0 --uname root --gname root \
      --options='!all,use-set,type,uid,gid,mode,time,size,sha256,link' \
      --null --files-from - --exclude .MTREE \
    | gzip -c -f -n >"$mtree"
  mv "$mtree" .MTREE
  touch -d "@${build_date}" .MTREE
  printf '%s\0' **/* \
    | bsdtar --no-fflags --no-read-sparse \
      --uid 0 --gid 0 --uname root --gname root \
      -cnf - --null --files-from - \
    | zstd --quiet --force -T0 -19 -o "$output_dir/$package_name"
)

echo "$output_dir/$package_name"
