#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT

install_dir="$test_root/bin"
mock_dir="$test_root/mock-bin"
payload="$test_root/payload"
mkdir -p "$install_dir" "$mock_dir"

if [ "$#" -eq 0 ]; then
  printf '#!/bin/sh\necho new\n' > "$payload"
  printf '#!/bin/sh\necho old\n' > "$install_dir/dbr"
else
  cp "$1" "$payload"
  cp /usr/bin/true "$install_dir/dbr"
fi
chmod 0755 "$payload" "$install_dir/dbr"
"$install_dir/dbr" >/dev/null
ln "$install_dir/dbr" "$install_dir/dbr-open"
ln "$install_dir/dbr" "$test_root/old-running"
old_inode=$(ls -i "$install_dir/dbr" | awk '{print $1}')

cat > "$mock_dir/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo Darwin ;;
  -m) echo arm64 ;;
  *) exit 1 ;;
esac
EOF

cat > "$mock_dir/curl" <<'EOF'
#!/bin/sh
destination=""
url=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      destination="$2"
      shift 2
      ;;
    -* ) shift ;;
    *)
      url="$1"
      shift
      ;;
  esac
done

case "$url" in
  *.sha256)
    checksum=$(shasum -a 256 "$TEST_PAYLOAD" | awk '{print $1}')
    printf '%s  dbr-aarch64-apple-darwin\n' "$checksum" > "$destination"
    ;;
  *) cp "$TEST_PAYLOAD" "$destination" ;;
esac
EOF

chmod 0755 "$mock_dir/uname" "$mock_dir/curl"

PATH="$mock_dir:$PATH" \
  DBR_VERSION=v-test \
  DBR_INSTALL_DIR="$install_dir" \
  TEST_PAYLOAD="$payload" \
  bash "$repo_root/scripts/install.sh"

new_inode=$(ls -i "$install_dir/dbr" | awk '{print $1}')
if [ "$new_inode" = "$old_inode" ]; then
  echo "installer overwrote the existing inode" >&2
  exit 1
fi

if ! [ "$install_dir/dbr" -ef "$install_dir/dbr-open" ]; then
  echo "dbr-open is not a hardlink to the installed binary" >&2
  exit 1
fi

if ! cmp -s "$payload" "$install_dir/dbr"; then
  echo "new binary was not installed" >&2
  exit 1
fi

if [ "$#" -eq 0 ] && [ "$("$install_dir/dbr")" != "new" ]; then
  echo "new binary does not execute" >&2
  exit 1
fi

if [ "$#" -gt 0 ]; then
  "$install_dir/dbr" --version
fi

if [ "$#" -eq 0 ]; then
  if [ "$("$test_root/old-running")" != "old" ]; then
    echo "old executable inode was modified in place" >&2
    exit 1
  fi
elif ! "$test_root/old-running" >/dev/null; then
  echo "old executable inode was modified in place" >&2
  exit 1
fi
