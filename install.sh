#!/usr/bin/env bash
# Spoke universal installer — Linux (all distros) + macOS.
#
# Install latest release (or update an existing install):
#   curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash
#
# Options:
#   curl -fsSL .../install.sh | bash -s -- --variant cpu      # CPU-only build (no GPU)
#   curl -fsSL .../install.sh | bash -s -- --variant cuda     # NVIDIA CUDA (native pkg + driver help)
#   curl -fsSL .../install.sh | bash -s -- --system           # install to /usr/local/bin (needs sudo)
#   curl -fsSL .../install.sh | bash -s -- --uninstall        # remove installed files
#   bash install.sh --binary ./src-tauri/target/release/spoke --variant cuda  # test a local build
#   SPOKE_VERSION=v0.3 bash install.sh                        # pin a version
#
# How it works: vulkan/cpu install the prebuilt AppImage (no root, no
# distro-specific packages, no build). There is no CUDA AppImage by design —
# bundling the CUDA runtime would be enormous and linuxdeploy can't resolve
# it (see .github/workflows/release.yml) — so --variant cuda installs the
# native package (.deb/.rpm) and guides you through the NVIDIA
# driver/runtime deps with your own package manager. Where no native package
# exists (Arch), it installs the prebuilt linux-cuda .deb plus NVIDIA's own
# 12.6 userspace runtime as a sidecar -- still no build, no AUR.
set -euo pipefail

REPO="${SPOKE_REPO:-Rainhexer/Spoke}"
VERSION="${SPOKE_VERSION:-latest}"
VARIANT="vulkan"          # vulkan | cpu | cuda  (cuda = native NVIDIA path)
SYSTEM=0                  # 1 = /usr/local/bin + /usr/share, needs root
YES=0                     # 1 = never prompt (assume yes)
BINARY=""                  # local binary to install instead of the release
UNINSTALL=0

usage() {
  cat <<EOF
Usage: install.sh [--variant vulkan|cpu|cuda] [--system] [--version TAG] [--yes] [--uninstall] [--help]

  --variant vulkan|cpu|cuda  Linux build flavour (default: vulkan — any AMD/Intel/NVIDIA GPU).
                        cpu works everywhere, slowest transcription.
                        cuda = max NVIDIA performance (.deb/.rpm native;
                        Arch via prebuilt .deb + NVIDIA 12.6 sidecar, ~300 MB).
  --system              Install to /usr/local/bin + /usr/share (needs sudo).
                        Default: ~/.local/bin + ~/.local/share (no root).
                        (cuda always uses system packages and needs sudo.)
  --version TAG         Install a specific release, e.g. --version v0.3.
                        Env: SPOKE_VERSION=v0.3
  --binary PATH         Install a local binary instead of downloading the
                        release (for testing local builds; Linux only).
                        Only with local runs: bash install.sh --binary ...
  --yes, -y             Never prompt; assume yes.
  --uninstall           Remove Spoke installed by this script.
  --help                Show this help.

Re-running the script updates an existing install to the requested version.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --variant) VARIANT="${2:-}"; shift 2 ;;
    --variant=*) VARIANT="${1#*=}"; shift ;;
    --system) SYSTEM=1; shift ;;
    --yes|-y) YES=1; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --binary) BINARY="$2"; shift 2 ;;
    --binary=*) BINARY="${1#*=}"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) echo "error: unknown flag: $1" >&2; usage >&2; exit 1 ;;
  esac
done

case "$VARIANT" in
  vulkan|cpu|cuda) ;;
  *) echo "error: --variant must be vulkan, cpu, or cuda, got: $VARIANT" >&2; exit 1 ;;
esac

OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" = "Darwin" ]; then
  if [ "$ARCH" != "arm64" ] && [ "$ARCH" != "aarch64" ]; then
    echo "note: Spoke ships macOS builds for Apple Silicon; Intel Macs must build CPU-only from source (see BUILD.md)." >&2
  fi
elif [ "$OS" != "Linux" ]; then
  echo "error: this script supports Linux and macOS. On Windows download the .msi/.exe from https://github.com/$REPO/releases/latest" >&2
  exit 1
fi
if [ "$OS" = "Linux" ] && [ "$ARCH" != "x86_64" ]; then
  echo "error: prebuilt Linux binaries are x86_64 only (got $ARCH). Build from source, see BUILD.md." >&2
  exit 1
fi

# Install locations (XDG, no root required by default).
if [ "$SYSTEM" -eq 1 ]; then
  BIN_DIR="/usr/local/bin"
  APP_DIR="/usr/share/applications"
  ICON_DIR="/usr/share/icons/hicolor/128x128/apps"
else
  BIN_DIR="${HOME}/.local/bin"
  APP_DIR="${HOME}/.local/share/applications"
  ICON_DIR="${HOME}/.local/share/icons/hicolor/128x128/apps"
fi

if [ "$UNINSTALL" -eq 1 ]; then
  rm -f "$BIN_DIR/spoke" "$APP_DIR/spoke.desktop" "$ICON_DIR/spoke.png"
  if [ "$OS" = "Darwin" ]; then rm -rf "/Applications/Spoke.app"; fi
  # System-wide files from --variant cuda (native deb/rpm or Arch sidecar path).
  if [ -e /usr/local/bin/spoke ] || [ -d /usr/local/lib/spoke ] || [ -e /usr/share/applications/spoke.desktop ]; then
    if command -v sudo >/dev/null 2>&1; then
      sudo rm -f /usr/local/bin/spoke /usr/share/applications/spoke.desktop \
        /usr/share/icons/hicolor/32x32/apps/spoke.png \
        /usr/share/icons/hicolor/128x128/apps/spoke.png \
        /usr/share/icons/hicolor/256x256/apps/spoke.png
      sudo rm -rf /usr/local/lib/spoke
      echo "Removed system-wide CUDA install (/usr/local)."
    else
      echo "warning: system-wide files exist but sudo is missing; remove manually:" >&2
      echo "  /usr/local/bin/spoke /usr/local/lib/spoke /usr/share/applications/spoke.desktop" >&2
    fi
  fi
  echo "Uninstalled Spoke."
  exit 0
fi

have() { command -v "$1" >/dev/null 2>&1; }
fetch() { # fetch <url> <outfile> — progress bar on a terminal, quiet otherwise
  if [ -t 2 ]; then
    if have curl; then curl -fSL --progress-bar -o "$2" "$1";
    elif have wget; then wget --show-progress -qO "$2" "$1";
    else echo "error: need curl or wget" >&2; exit 1; fi
  else
    if have curl; then curl -fsSL -o "$2" "$1";
    elif have wget; then wget -qO "$2" "$1";
    else echo "error: need curl or wget" >&2; exit 1; fi
  fi
}

# Resolve version tag + asset list via the GitHub API (no jq needed).
API="https://api.github.com/repos/$REPO/releases"
if [ "$VERSION" = "latest" ]; then
  JSON="$(have curl && curl -fsSL "$API/latest" || wget -qO- "$API/latest")"
else
  JSON="$(have curl && curl -fsSL "$API/tags/$VERSION" || wget -qO- "$API/tags/$VERSION")"
fi
TAG="$(printf '%s' "$JSON" | grep -m1 '"tag_name"' | sed 's/.*"tag_name": *"//;s/".*//')"
[ -n "$TAG" ] || { echo "error: could not resolve release (check SPOKE_VERSION / network)" >&2; exit 1; }
URLS="$(printf '%s' "$JSON" | grep '"browser_download_url"' | sed 's/.*"browser_download_url": *"//;s/".*//')"

pick_asset() { printf '%s\n' "$URLS" | grep -E "$1" | head -n1; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

if [ "$OS" = "Darwin" ]; then
  if [ "$VARIANT" = "cuda" ]; then
    echo "error: --variant cuda is Linux-only. macOS uses its own Apple Silicon build." >&2; exit 1
  fi
  if [ -n "$BINARY" ]; then
    echo "error: --binary is only supported on Linux (AppImage and Arch CUDA paths)." >&2; exit 1
  fi
  ASSET="$(pick_asset 'macos-arm64.*\.dmg$')"
  [ -n "$ASSET" ] || { echo "error: no macOS .dmg found in $TAG" >&2; exit 1; }
  echo "Installing Spoke $TAG for macOS…"
  fetch "$ASSET" "$TMP/Spoke.dmg"
  MNT="$(mktemp -d /tmp/spoke-dmg.XXXX)"
  hdiutil attach "$TMP/Spoke.dmg" -nobrowse -readonly -mountpoint "$MNT" >/dev/null
  trap 'hdiutil detach "$MNT" >/dev/null 2>&1 || true; rm -rf "$TMP" "$MNT"' EXIT
  rm -rf "/Applications/Spoke.app"
  cp -R "$MNT/Spoke.app" "/Applications/Spoke.app"
  hdiutil detach "$MNT" >/dev/null
  trap 'rm -rf "$TMP"' EXIT
  echo "Done. Open /Applications/Spoke.app"
  echo "Note: Spoke is not notarized yet — first launch: System Settings → Privacy & Security → Open Anyway, then grant Microphone + Accessibility."
  exit 0
fi

# --- Linux CUDA: native package + per-distro driver/runtime guidance ---
# There is no CUDA AppImage (see header), so this path uses your package
# manager: .deb/.rpm straight from the release; Arch builds from source.
detect_pm() {
  if have apt-get; then echo apt
  elif have dnf; then echo dnf
  elif have pacman; then echo pacman
  elif have zypper; then echo zypper
  else echo none; fi
}

confirm() { # confirm <prompt> — true if --yes or user answers y
  [ "$YES" -eq 1 ] && return 0
  printf '%s [y/N] ' "$1"
  read -r ans </dev/tty || return 1
  [ "$ans" = "y" ] || [ "$ans" = "Y" ]
}

need_sudo() {
  have sudo || { echo "error: this step needs sudo, but sudo is not installed." >&2; exit 1; }
}

fetch_stdout() { # fetch_stdout <url>
  if have curl; then curl -fsSL "$1";
  elif have wget; then wget -qO- "$1";
  else echo "error: need curl or wget" >&2; exit 1; fi
}

list_deb_data() { # list_deb_data <deb> — list members of the inner data.tar
  if have bsdtar; then
    _t="$(mktemp -d)"
    bsdtar -xf "$1" -C "$_t"
    _d="$(ls "$_t"/data.tar.* | head -n1)"
    bsdtar -tf "$_d"
    rm -rf "$_t"
  else
    _d="$(ar t "$1" | grep '^data\.tar' | head -n1)"
    ar p "$1" "$_d" | tar -tf -
  fi
}

deb_data_extract() { # deb_data_extract <deb> <dest> [members...] (empty = all)
  _deb="$1"; _dest="$2"; shift 2
  mkdir -p "$_dest"
  if have bsdtar; then
    _t="$(mktemp -d)"
    bsdtar -xf "$_deb" -C "$_t"
    _d="$(ls "$_t"/data.tar.* | head -n1)"
    if [ "$#" -eq 0 ]; then bsdtar -xf "$_d" -C "$_dest";
    else bsdtar -xf "$_d" -C "$_dest" "$@"; fi
    rm -rf "$_t"
  else
    _d="$(ar t "$_deb" | grep '^data\.tar' | head -n1)"
    ar p "$_deb" "$_d" | tar -xf - -C "$_dest" "$@"
  fi
}

pick_nv_deb() { # pick_nv_deb <Packages-text> <pkg-name> — newest 12.6 ./x.deb path
  printf '%s\n' "$1" | awk -v pkg="Package: $2" '
    BEGIN { RS=""; FS="\n" }
    $1 == pkg && $0 ~ /Version: 12\.6/ {
      for (i=1; i<=NF; i++) if ($i ~ /^Filename: /) { sub(/^Filename: /, "", $i); last=$i }
    }
    END { print last }'
}

install_cuda_linux() {
  PM="$(detect_pm)"
  echo "Installing Spoke $TAG (linux-cuda) via $PM…"
  echo "Note: CUDA needs the proprietary NVIDIA driver + CUDA runtime libs on the host."
  echo "No NVIDIA GPU / prefer zero setup? Use --variant vulkan instead (GPU via Vulkan, incl. NVIDIA)."
  echo
  if [ -n "$BINARY" ] && [ "$PM" != "pacman" ]; then
    echo "error: --binary with --variant cuda is only supported on Arch (pacman)." >&2
    echo "On deb/rpm distros install the native package instead; other builds: --variant vulkan|cpu with --binary." >&2
    exit 1
  fi

  case "$PM" in
    apt)
      if ! have nvidia-smi; then
        cat <<'EOF'
No NVIDIA driver detected. Install it with your package manager first, reboot, then re-run this script:
  sudo apt update && sudo apt install -y nvidia-driver nvidia-cuda-toolkit
  # alternative on Ubuntu: sudo ubuntu-drivers autoinstall
EOF
        exit 1
      fi
      ASSET="$(pick_asset 'linux-cuda.*\.deb$')"
      [ -n "$ASSET" ] || { echo "error: no linux-cuda .deb found in $TAG" >&2; exit 1; }
      need_sudo
      fetch "$ASSET" "$TMP/spoke-cuda.deb"
      confirm "Install with: sudo apt install -y $TMP/spoke-cuda.deb?" || exit 0
      sudo apt install -y "$TMP/spoke-cuda.deb"
      echo "Done: spoke $TAG (cuda). Verify with: nvidia-smi && spoke"
      echo "Missing CUDA libs at runtime? sudo apt install -y nvidia-cuda-toolkit, then reinstall."
      ;;
    dnf)
      if ! have nvidia-smi; then
        cat <<'EOF'
No NVIDIA driver detected. Enable RPM Fusion, then install the driver + CUDA runtime, reboot, and re-run:
  sudo dnf install -y akmod-nvidia xorg-x11-drv-nvidia-cuda
  # see: https://rpmfusion.org/Howto/NVIDIA
EOF
        exit 1
      fi
      ASSET="$(pick_asset 'linux-cuda.*\.rpm$')"
      [ -n "$ASSET" ] || { echo "error: no linux-cuda .rpm found in $TAG" >&2; exit 1; }
      need_sudo
      fetch "$ASSET" "$TMP/spoke-cuda.rpm"
      confirm "Install with: sudo dnf install -y $TMP/spoke-cuda.rpm?" || exit 0
      sudo dnf install -y "$TMP/spoke-cuda.rpm"
      echo "Done: spoke $TAG (cuda). Verify with: nvidia-smi && spoke"
      ;;
    zypper)
      if ! have nvidia-smi; then
        cat <<'EOF'
No NVIDIA driver detected. Add the NVIDIA repository for your openSUSE version,
then install the driver + CUDA runtime, reboot, and re-run:
  sudo zypper install -y nvidia-driver-G06-kmp-default cuda
  # see: https://en.opensuse.org/SDB:NVIDIA_drivers
EOF
        exit 1
      fi
      ASSET="$(pick_asset 'linux-cuda.*\.rpm$')"
      [ -n "$ASSET" ] || { echo "error: no linux-cuda .rpm found in $TAG" >&2; exit 1; }
      need_sudo
      fetch "$ASSET" "$TMP/spoke-cuda.rpm"
      confirm "Install with: sudo zypper install -y $TMP/spoke-cuda.rpm?" || exit 0
      sudo zypper install -y "$TMP/spoke-cuda.rpm"
      echo "Done: spoke $TAG (cuda). Verify with: nvidia-smi && spoke"
      ;;
    pacman)
      # No native Arch package and no CUDA AppImage, so: install the prebuilt
      # linux-cuda .deb plus NVIDIA's own 12.6 userspace runtime as a sidecar.
      # (The Ubuntu-built binary needs versioned libcudart/cuBLAS .so.12
      # symbols, which Arch's CUDA 13 system libs no longer satisfy.)
      # No build, no AUR — about 300 MB download, needs sudo for /usr/local.
      need_sudo
      if ! have nvidia-smi; then
        cat <<'EOF'
No NVIDIA driver detected. Install it with your package manager, reboot, then
re-run this script:
  sudo pacman -S --needed nvidia nvidia-utils
EOF
        exit 1
      fi
      echo "Detected: NVIDIA driver $(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -n1)."
      if [ -d /opt/cuda ]; then
        echo "Detected: CUDA toolkit at /opt/cuda."
      fi
      if ! have awk || ! have gzip; then
        echo "Installing archive tools..."
        sudo pacman -S --needed gawk gzip
      fi
      # Base GUI libs the binary links against.
      MISSING=""
      for pkg in webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg openssl alsa-lib xdotool gst-plugins-base gst-plugins-good; do
        if ! pacman -Qq "$pkg" >/dev/null 2>&1; then MISSING="$MISSING $pkg"; fi
      done
      if [ -n "$MISSING" ]; then
        echo "Missing system libraries:$MISSING"
        if confirm "Install them now (sudo pacman -S --needed$ MISSING)?"; then
          # shellcheck disable=SC2086
          sudo pacman -S --needed $MISSING
        else
          exit 0
        fi
      fi
      if ! have bsdtar && ! have ar; then
        echo "Installing archive tools..."
        sudo pacman -S --needed libarchive
      fi
      SPOKE_BIN=""; USE_DEB_ASSETS=0; SPOKE_DEB=""
      NEED12=1
      if [ -n "$BINARY" ]; then
        if [ ! -f "$BINARY" ]; then echo "error: --binary file not found: $BINARY" >&2; exit 1; fi
        if ! have readelf; then
          echo "Installing archive tools..."
          sudo pacman -S --needed binutils
        fi
        echo "Using local binary: $BINARY (skipping release download)."
        SPOKE_BIN="$BINARY"
        # A locally built binary links the host CUDA: sidecar only if it needs .so.12.
        NEED12=0
        if readelf -d "$SPOKE_BIN" 2>/dev/null | grep -qE 'libcudart\.so\.12|libcublas\.so\.12'; then NEED12=1; fi
        if [ "$NEED12" -eq 0 ]; then
          echo "Binary links newer CUDA system libs -- no 12.x sidecar needed."
          MISS13="$(ldd "$SPOKE_BIN" 2>/dev/null | grep 'not found' || true)"
          if [ -n "$MISS13" ]; then
            echo "error: the local binary needs libraries missing on this system:" >&2
            printf '%s\n' "$MISS13" >&2
            echo "Install them (e.g. sudo pacman -S cuda) and re-run." >&2
            exit 1
          fi
        fi
      else
        SPOKE_DEB="$(pick_asset 'linux-cuda.*\.deb$')"
        if [ -z "$SPOKE_DEB" ]; then echo "error: no linux-cuda .deb found in $TAG" >&2; exit 1; fi
      fi
      # The prebuilt binary needs real CUDA 12.x libraries. If the host linker
      # already provides them, use them and skip the sidecar download below.
      # (Note: .so.12 symlinks pointing at .so.13 files do NOT count -- the
      # loader checks versioned symbols, so those still fail. Only real 12.x
      # libs, visible in ldconfig, are accepted.)
      HAVE_HOST_CUDART=0; HAVE_HOST_CUBLAS=0
      if [ "$NEED12" -eq 0 ]; then HAVE_HOST_CUDART=1; HAVE_HOST_CUBLAS=1; fi
      if ldconfig -p 2>/dev/null | grep -qE 'libcudart\.so\.12 \('; then HAVE_HOST_CUDART=1; fi
      if ldconfig -p 2>/dev/null | grep -qE 'libcublas\.so\.12 \('; then HAVE_HOST_CUBLAS=1; fi
      NVBASE="https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64"
      CUDART_PATH=""; CUBLAS_PATH=""
      if [ "$HAVE_HOST_CUDART" -eq 1 ] && [ "$HAVE_HOST_CUBLAS" -eq 1 ]; then
        if [ "$NEED12" -eq 1 ]; then echo "Detected: host CUDA 12.x runtime (cudart + cublas) -- no sidecar download needed."; else echo "Local binary uses the system CUDA libs -- no sidecar needed."; fi
      else
        if [ "$HAVE_HOST_CUDART" -eq 1 ]; then echo "Detected: host cudart 12.x -- skipping its download."; fi
        if [ "$HAVE_HOST_CUBLAS" -eq 1 ]; then echo "Detected: host cublas 12.x -- skipping its download."; fi
        echo "Resolving the missing CUDA 12.6 userspace runtime from NVIDIA..."
        PKGS="$(fetch_stdout "$NVBASE/Packages.gz" | gzip -dc)"
        if [ "$HAVE_HOST_CUDART" -eq 0 ]; then
          CUDART_PATH="$(pick_nv_deb "$PKGS" cuda-cudart-12-6)"
          if [ -z "$CUDART_PATH" ]; then echo "error: could not resolve cudart 12.6 from NVIDIA." >&2; exit 1; fi
          CUDART_PATH="${CUDART_PATH#./}"
        fi
        if [ "$HAVE_HOST_CUBLAS" -eq 0 ]; then
          CUBLAS_PATH="$(pick_nv_deb "$PKGS" libcublas-12-6)"
          if [ -z "$CUBLAS_PATH" ]; then echo "error: could not resolve cublas 12.6 from NVIDIA." >&2; exit 1; fi
          CUBLAS_PATH="${CUBLAS_PATH#./}"
        fi
      fi
      echo "Packages:"
      if [ -n "$BINARY" ]; then
        echo "  $BINARY  (local binary, no download)"
      else
        echo "  $(basename "$SPOKE_DEB")  (Spoke linux-cuda, ~40 MB)"
      fi
      DL_SIZE=""
      if [ -z "$BINARY" ]; then DL_SIZE="~40 MB"; fi
      if [ -n "$CUDART_PATH" ]; then echo "  $(basename "$CUDART_PATH")  (NVIDIA cudart 12.6)"; fi
      if [ -n "$CUBLAS_PATH" ]; then echo "  $(basename "$CUBLAS_PATH")  (NVIDIA cublas 12.6, ~250 MB)"; DL_SIZE="~300 MB"; fi
      if [ -n "$DL_SIZE" ]; then
        if ! confirm "Download $DL_SIZE and install the CUDA build system-wide?"; then exit 0; fi
      else
        if ! confirm "Install the local build system-wide (needs sudo)?"; then exit 0; fi
      fi
      mkdir -p "$TMP/nv"
      echo "Extracting..."
      if [ -z "$BINARY" ]; then
        fetch "$SPOKE_DEB" "$TMP/spoke-cuda.deb"
        deb_data_extract "$TMP/spoke-cuda.deb" "$TMP/spoke"
        SPOKE_BIN="$TMP/spoke/usr/bin/spoke"
        USE_DEB_ASSETS=1
      fi
      if [ -n "$CUDART_PATH" ]; then
        fetch "$NVBASE/$CUDART_PATH" "$TMP/cudart.deb"
        # shellcheck disable=SC2046
        deb_data_extract "$TMP/cudart.deb" "$TMP/nv" $(list_deb_data "$TMP/cudart.deb" | grep -E '/libcudart\.so\.12(\.[0-9]+)*$')
      fi
      if [ -n "$CUBLAS_PATH" ]; then
        fetch "$NVBASE/$CUBLAS_PATH" "$TMP/cublas.deb"
        # shellcheck disable=SC2046
        deb_data_extract "$TMP/cublas.deb" "$TMP/nv" $(list_deb_data "$TMP/cublas.deb" | grep -E '/libcublas(Lt)?\.so\.12(\.[0-9]+)*$')
      fi
      echo "Installing to /usr/local (needs sudo)..."
      sudo install -Dm755 "$SPOKE_BIN" /usr/local/lib/spoke/spoke
      sudo mkdir -p /usr/local/lib/spoke/cuda12 /usr/local/lib/spoke/compat
      for f in "$TMP"/nv/usr/local/cuda-12.6/targets/x86_64-linux/lib/*; do
        if [ -e "$f" ] || [ -L "$f" ]; then
          if [ -L "$f" ]; then sudo cp -d "$f" /usr/local/lib/spoke/cuda12/
          else sudo install -Dm644 "$f" "/usr/local/lib/spoke/cuda12/$(basename "$f")"; fi
        fi
      done
      if [ ! -e /usr/lib/libxdo.so.3 ] && [ -e /usr/lib/libxdo.so.4 ]; then
        sudo ln -sf /usr/lib/libxdo.so.4 /usr/local/lib/spoke/compat/libxdo.so.3
        echo "Note: using system libxdo.so.4 as libxdo.so.3 (ABI-compatible shim)."
      fi
      sudo tee /usr/local/bin/spoke >/dev/null <<'EOF'
#!/bin/sh
# Installed by Spoke install.sh (linux-cuda): prebuilt binary + CUDA 12.6 sidecar.
export LD_LIBRARY_PATH="/usr/local/lib/spoke/cuda12:/usr/local/lib/spoke/compat${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec /usr/local/lib/spoke/spoke "$@"
EOF
      sudo chmod +x /usr/local/bin/spoke
      if [ "$USE_DEB_ASSETS" -eq 1 ]; then
        sudo install -Dm644 "$TMP/spoke/usr/share/applications/Spoke.desktop" /usr/share/applications/spoke.desktop
        sudo install -Dm644 "$TMP/spoke/usr/share/icons/hicolor/32x32/apps/spoke.png" /usr/share/icons/hicolor/32x32/apps/spoke.png
        sudo install -Dm644 "$TMP/spoke/usr/share/icons/hicolor/128x128/apps/spoke.png" /usr/share/icons/hicolor/128x128/apps/spoke.png
        sudo install -Dm644 "$TMP/spoke/usr/share/icons/hicolor/256x256@2/apps/spoke.png" /usr/share/icons/hicolor/256x256/apps/spoke.png
      else
        sudo tee /usr/share/applications/spoke.desktop >/dev/null <<'EOF'
[Desktop Entry]
Name=Spoke
Comment=Push-to-talk voice dictation
Exec=/usr/local/bin/spoke
Icon=spoke
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
EOF
        ICON_SRC=""
        for cand in "./src-tauri/icons/128x128.png" "${0%/*}/src-tauri/icons/128x128.png"; do
          if [ -f "$cand" ]; then ICON_SRC="$cand"; break; fi
        done
        if [ -n "$ICON_SRC" ]; then
          sudo install -Dm644 "$ICON_SRC" /usr/share/icons/hicolor/128x128/apps/spoke.png
        else
          echo "Note: no icon found (run from the repo root to install one)."
        fi
      fi
      if have gtk-update-icon-cache; then sudo gtk-update-icon-cache -f /usr/share/icons/hicolor >/dev/null 2>&1 || true; fi
      if have update-desktop-database; then sudo update-desktop-database /usr/share/applications >/dev/null 2>&1 || true; fi
      if LD_LIBRARY_PATH="/usr/local/lib/spoke/cuda12:/usr/local/lib/spoke/compat" ldd /usr/local/lib/spoke/spoke 2>/dev/null | grep -q "not found"; then
        echo "warning: some libraries are still unresolved:" >&2
        LD_LIBRARY_PATH="/usr/local/lib/spoke/cuda12:/usr/local/lib/spoke/compat" ldd /usr/local/lib/spoke/spoke 2>/dev/null | grep "not found" >&2
        exit 1
      fi
      echo "Done: CUDA build installed ($TAG). All libraries resolve. Launch with: spoke"
      echo "First run: click the bubble -> Settings -> download a speech model."
      ;;
    none)
      cat <<EOF
No supported package manager detected (looked for apt-get, dnf, pacman, zypper),
so this script can't install the driver, runtime, or native package for you.

If you want CUDA anyway, your system needs these installed by hand first:
  1. An NVIDIA GPU with the proprietary NVIDIA driver
     (check with: nvidia-smi -- if missing, install your distro's NVIDIA
      driver package and reboot)
  2. The CUDA runtime libraries: cudart + cuBLAS (12.x series -- the $TAG
     builds use CUDA 12.6)
     (usually your distro's cuda / nvidia-cuda-toolkit package, or
      https://developer.nvidia.com/cuda-downloads)
  3. Spoke's base libraries: webkit2gtk-4.1, gtk3, libappindicator,
     librsvg, openssl, alsa-lib, gst-plugins-base + gst-plugins-good
     (package names vary by distro -- see BUILD.md "System dependencies")

What would you like to do?
  1) Continue: download the native CUDA packages (.deb + .rpm) here and
     I'll install them myself
  2) Install the Vulkan build instead (AppImage, GPU incl. NVIDIA, no
     package manager or root needed)
  3) Cancel
EOF
      choice=""
      if [ "$YES" -eq 1 ]; then
        choice="1"
      else
        printf 'Choice [1/2/3, default 3]: '
        read -r choice </dev/tty || choice="3"
      fi
      case "$choice" in
        1|continue|CONTINUE)
          DEB="$(pick_asset 'linux-cuda.*\.deb$')"
          RPM="$(pick_asset 'linux-cuda.*\.rpm$')"
          [ -n "$DEB$RPM" ] || { echo "error: no linux-cuda packages found in $TAG" >&2; exit 1; }
          DLDIR="spoke-cuda-${TAG}"
          mkdir -p "$DLDIR"
          if [ -n "$DEB" ]; then echo "Downloading .deb..."; fetch "$DEB" "$DLDIR/$(basename "$DEB")"; fi
          if [ -n "$RPM" ]; then echo "Downloading .rpm..."; fetch "$RPM" "$DLDIR/$(basename "$RPM")"; fi
          cat <<EOF2
Downloaded to ./$DLDIR/. Install with your distro's tool, then launch: spoke
  Debian-family:         sudo dpkg -i $DLDIR/*.deb && sudo apt-get install -f
  Fedora/openSUSE-family: sudo rpm -i $DLDIR/*.rpm   (or: sudo dnf install ./$DLDIR/*.rpm)
You need only one package -- .deb xor .rpm, whichever matches your distro.
Make sure the driver + CUDA runtime above are in place first.
EOF2
          ;;
        2|vulkan|VULKAN)
          VARIANT="vulkan"
          return 2
          ;;
        *)
          echo "Cancelled."
          exit 1
          ;;
      esac
      ;;
  esac
  exit 0
}

# --- Linux: universal AppImage install ---
if [ "$VARIANT" = "cuda" ]; then
  rc=0; install_cuda_linux || rc=$?
  if [ "$VARIANT" = "vulkan" ]; then
    echo "Continuing with the Vulkan AppImage instead..."
  else
    exit "$rc"
  fi
fi
ASSET=""
if [ -n "$BINARY" ]; then
  if [ ! -f "$BINARY" ]; then echo "error: --binary file not found: $BINARY" >&2; exit 1; fi
  echo "Installing local binary $BINARY (linux-$VARIANT)..."
  SRCBIN="$BINARY"
else
  ASSET="$(pick_asset "linux-${VARIANT}.*\\.AppImage$")"
  if [ -z "$ASSET" ] && [ "$VARIANT" = "vulkan" ]; then
    echo "note: no vulkan AppImage in $TAG, falling back to cpu." >&2
    ASSET="$(pick_asset 'linux-cpu.*\.AppImage$')"
  fi
  [ -n "$ASSET" ] || { echo "error: no linux AppImage found in $TAG" >&2; exit 1; }
  echo "Installing Spoke $TAG (linux-$VARIANT)..."
  fetch "$ASSET" "$TMP/spoke.AppImage"
  SRCBIN="$TMP/spoke.AppImage"
fi

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
cp "$SRCBIN" "$BIN_DIR/spoke"
chmod +x "$BIN_DIR/spoke"

# Icon: fetch from the release tag (no FUSE needed, unlike --appimage-extract).
fetch "https://raw.githubusercontent.com/$REPO/$TAG/src-tauri/icons/128x128.png" "$ICON_DIR/spoke.png" || true

cat > "$APP_DIR/spoke.desktop" <<EOF
[Desktop Entry]
Name=Spoke
Comment=Push-to-talk voice dictation
Exec=$BIN_DIR/spoke
Icon=spoke
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
EOF

have update-desktop-database && update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
have gtk-update-icon-cache && gtk-update-icon-cache -f "${ICON_DIR%/128x128/apps}" >/dev/null 2>&1 || true

echo "Done: $BIN_DIR/spoke ($TAG)"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR is not on PATH. Add: export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac
[ "$VARIANT" = "cpu" ] || echo "GPU build: transcription uses Vulkan when a compatible GPU/driver is present, else CPU."
echo "First run: click the bubble → Settings → download a speech model. Wayland issue? Try: GDK_BACKEND=x11 spoke"
