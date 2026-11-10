FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y \
    libgtk-3-0t64 libwebkit2gtk-4.1-0 xdg-utils desktop-file-utils \
    gnome-keyring dbus-x11 libglib2.0-bin xvfb xauth webkit2gtk-driver python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
