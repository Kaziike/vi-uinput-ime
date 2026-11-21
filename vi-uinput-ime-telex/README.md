# `vi-uinput-ime-telex` — Bộ gõ Telex (evdev + uinput + Custom XKB)


## 1. Biên dịch & Kiểm thử

```bash
cargo test
cargo build --release
```

## 2. Cài đặt phân quyền (`uinput` + `udev` + `systemd`)

```bash
# Tạo user/group hệ thống (bỏ qua nếu đã tạo)
sudo groupadd --system vi-ime 2>/dev/null || true
sudo useradd --system --gid vi-ime --no-create-home --shell /usr/sbin/nologin vi-ime 2>/dev/null || true

# Nạp module uinput và tự động khi khởi động máy
echo "uinput" | sudo tee /etc/modules-load.d/uinput.conf
sudo modprobe uinput

# Cài binary, config và XKB keymap
sudo install -Dm755 target/release/vi-uinput-ime-telex /usr/local/bin/vi-uinput-ime-telex
sudo install -Dm644 config/default_config.toml /etc/vi-uinput-ime-telex/config.toml
sudo install -Dm644 config/vn_virtual.xkb /etc/vi-uinput-ime/vn_virtual.xkb

# Cài udev rule
sudo install -Dm644 deploy/99-vi-uinput-ime.rules /etc/udev/rules.d/99-vi-uinput-ime.rules
sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=misc --subsystem-match=input

# Cài và bật systemd service
sudo install -Dm644 deploy/vi-uinput-ime-telex.service /etc/systemd/system/vi-uinput-ime-telex.service
sudo systemctl daemon-reload
sudo systemctl disable --now vi-uinput-ime-vni.service 2>/dev/null || true
sudo systemctl enable --now vi-uinput-ime-telex.service
```

## 3. Kích hoạt layout XKB 134 ký tự (`vi_uinput`)

### GNOME / Mutter Wayland
```bash
/usr/local/bin/vi-uinput-ime-telex --install-gnome-xkb
gsettings set org.gnome.desktop.input-sources show-all-sources true
gsettings set org.gnome.desktop.input-sources sources "[('xkb', 'vi_uinput')]"
```

### KDE Plasma 6 / KWin Wayland
```bash
/usr/local/bin/vi-uinput-ime-telex --install-gnome-xkb
kwriteconfig6 --file kxkbrc --group Layout --key Use true
kwriteconfig6 --file kxkbrc --group Layout --key LayoutList vi_uinput
```

### Hyprland / Sway / X11

