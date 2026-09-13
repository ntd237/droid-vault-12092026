#!/bin/sh
# Build src-tauri/assets/dv_helper.dex from DvHelper.java.
# Requirements: JDK (javac) + Android SDK build-tools (d8) + platforms/<api>/android.jar.
# Auto-detects %LOCALAPPDATA%/Android/Sdk; override with ANDROID_SDK / JAVA_HOME if needed.
set -e
cd "$(dirname "$0")"

SDK="${ANDROID_SDK:-$LOCALAPPDATA/Android/Sdk}"
[ -n "$JAVA_HOME" ] && JAVAC="$JAVA_HOME/bin/javac" || JAVAC=javac

# Newest build-tools + newest platform available.
BT=$(ls "$SDK/build-tools" | sort -V | tail -1)
PLAT=$(ls "$SDK/platforms" | grep '^android-' | sort -V | tail -1)
JAR="$SDK/platforms/$PLAT/android.jar"
echo "javac=$JAVAC  build-tools=$BT  platform=$PLAT"

rm -rf build
mkdir -p build/classes build/dex
"$JAVAC" --release 8 -Xlint:-options -cp "$JAR" -d build/classes DvHelper.java
"$SDK/build-tools/$BT/d8.bat" --release --min-api 28 \
  --output build/dex build/classes/dv/DvHelper.class 2>/dev/null \
  || "$SDK/build-tools/$BT/d8.bat" --release --min-api 28 \
  --output build/dex build/classes/DvHelper.class

mkdir -p ../assets
cp build/dex/classes.dex ../assets/dv_helper.dex
ls -la ../assets/dv_helper.dex
echo "OK: ../assets/dv_helper.dex"
