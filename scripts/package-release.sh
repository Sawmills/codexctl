#!/usr/bin/env bash
set -euo pipefail

tag=$1
target=$2
label=$3
root=$PWD
release="target/${target}/release"
prefix="codexctl_${tag}_${label}"
assets=("${prefix}.tar.gz")

case "$target" in
  *-apple-darwin)
    uuid_file="${prefix}.uuid.txt"
    : > "$uuid_file"
    for name in codexctl codexctl-central; do
      binary="${release}/${name}"
      dwarf_files=("${binary}.dSYM/Contents/Resources/DWARF/"*)
      test "${#dwarf_files[@]}" -eq 1
      test -f "${dwarf_files[0]}"
      test -s "${dwarf_files[0]}"
      binary_uuid=$(dwarfdump --uuid "$binary")
      debug_uuid=$(dwarfdump --uuid "${binary}.dSYM")
      uuid=$(printf '%s\n' "$binary_uuid" | awk '/^UUID:/ {print $2}')
      test -n "$uuid"
      test "$uuid" = "$(printf '%s\n' "$debug_uuid" | awk '/^UUID:/ {print $2}')"
      printf '%s\n%s\n' "$binary_uuid" "$debug_uuid" >> "$uuid_file"
      strip -S "$binary"
    done
    (
      cd "$release"
      python3 -m zipfile -c "${root}/${prefix}.dSYM.zip" codexctl.dSYM codexctl-central.dSYM
    )
    assets+=("${prefix}.dSYM.zip" "$uuid_file")
    ;;
  *-unknown-linux-gnu)
    debug_files=()
    for name in codexctl codexctl-central; do
      binary="${release}/${name}"
      objcopy --only-keep-debug "$binary" "${binary}.debug"
      test -s "${binary}.debug"
      strip --strip-debug "$binary"
      objcopy --add-gnu-debuglink="${binary}.debug" "$binary"
      debug_files+=("${name}.debug")
      # Rust's packed split DWARF may also contain compilation units here.
      if [[ -f "${binary}.dwp" ]]; then
        debug_files+=("${name}.dwp")
      fi
    done
    tar -czf "${prefix}.debug.tar.gz" -C "$release" "${debug_files[@]}"
    assets+=("${prefix}.debug.tar.gz")
    ;;
  *)
    echo "Unsupported release target: $target" >&2
    exit 1
    ;;
esac

tar -czf "${prefix}.tar.gz" -C "$release" codexctl codexctl-central
for asset in "${assets[@]}"; do
  shasum -a 256 "$asset" > "${asset}.sha256"
done
