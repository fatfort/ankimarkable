set -e
. /opt/codex/*/*/environment-setup-* 2>/dev/null
cd /work
echo "CXX=$CXX"
QTCFLAGS=$(pkg-config --cflags Qt6Gui Qt6Core) || { echo "pkg-config Qt failed"; exit 1; }
QTLIBS=$(pkg-config --libs Qt6Gui Qt6Core)
CXXFLAGS="-std=c++17 -O2 -DNDEBUG -DBUILD_QT -fPIC -Isrc -Itinyxml2 $QTCFLAGS"
SRCS=$(find src -name '*.cpp' | grep -vE 'src/platform/(skia|gdi_win|cairo)/|src/samples/')
mkdir -p build-arm/obj
n=0
for f in $SRCS tinyxml2/tinyxml2.cpp; do
  o=build-arm/obj/$(echo "$f" | tr '/.' '__').o
  if [ ! -f "$o" ] || [ "$f" -nt "$o" ]; then
    $CXX $CXXFLAGS -c "$f" -o "$o" || { echo "FAILED: $f"; exit 1; }
  fi
  n=$((n+1))
done
echo "engine objects ready ($n)"
$CXX $CXXFLAGS example/mathpng.cpp build-arm/obj/*.o $QTLIBS -o build-arm/mathpng \
  || { echo "LINK FAILED"; exit 1; }
"$STRIP" --strip-unneeded build-arm/mathpng 2>/dev/null || aarch64-remarkable-linux-strip --strip-unneeded build-arm/mathpng 2>/dev/null || true
echo "=== built ==="
file build-arm/mathpng
ls -la build-arm/mathpng
