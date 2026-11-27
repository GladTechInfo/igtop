#!/bin/sh
# Exercise destination selection with a local release and no package changes.
set -eu
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir "$dir/tools" "$dir/private" "$dir/release"
asset=igtop-1.2.3-x86_64-linux
printf '%s\n' '#!/bin/sh' 'echo "igtop 1.2.3"' > "$dir/release/$asset"
(cd "$dir/release" && sha256sum "$asset" > SHA256SUMS)
install -m 755 "$dir/release/$asset" "$dir/tools/igtop"
cat > "$dir/tools/curl" <<'MOCK'
#!/bin/sh
cp "$FIXTURE/${4##*/}" "$3"
MOCK
# Simulate an installed Debian package and a user without elevation.
printf '%s\n' '#!/bin/sh' 'echo "installed 1.2.3-1"' > "$dir/tools/dpkg-query"
printf '%s\n' '#!/bin/sh' 'echo 1000' > "$dir/tools/id"
for tool in sudo doas apt-get; do
  printf '%s\n' '#!/bin/sh' 'exit 99' > "$dir/tools/$tool"
done
chmod +x "$dir/tools/"*
export PATH="$dir/tools:$PATH" FIXTURE="$dir/release"
sh install.sh --version 1.2.3 --bindir "$dir/private"
test "$("$dir/private/igtop" --version)" = 'igtop 1.2.3'
sh install.sh --uninstall --bindir "$dir/private"
test ! -e "$dir/private/igtop"
test -x "$dir/tools/igtop"
