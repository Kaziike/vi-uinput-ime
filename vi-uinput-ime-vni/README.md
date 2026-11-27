# `vi-uinput-ime-vni` — Bộ gõ VNI thuần Linux (evdev + uinput + Custom XKB)

Phiên bản chuyên biệt cho **VNI** (`1`=sắc, `2`=huyền, `3`=hỏi, `4`=ngã, `5`=nặng, `0`=xóa dấu, `6`=mũ, `7`=móc, `8`=trăng, `9`=đ), hỗ trợ **khôi phục từ khi xóa ngược (Word History Restoration, ring buffer 15 phần tử)**, **ghi đè dấu nguyên âm** (`Năng` + `6` → `Nâng`, `Nâng` + `8` → `Năng`) với **Minimal Diff** qua FIFO, gõ số không kẹt phím (`passthrough_held`), double-tap undo (`a11` → `a1`), và hủy lịch sử nghiêm ngặt khi đổi cửa sổ/workspace, click chuột, nhấn phím điều hướng hoặc tổ hợp `Ctrl`/`Alt`/`Super`.

## 1. Biên dịch & Kiểm thử

```bash
cd vi-uinput-ime-vni
cargo test
cargo build --release
```

## 2. Cài đặt phân quyền (`uinput` + `udev` + `systemd`)

```bash
# Tạo user/group hệ thống cô lập (bỏ qua nếu đã tạo)
sudo groupadd --system vi-ime 2>/dev/null || true
sudo useradd --system --gid vi-ime --no-create-home --shell /usr/sbin/nologin vi-ime 2>/dev/null || true

# Nạp module uinput ngay và tự động khi khởi động máy
echo "uinput" | sudo tee /etc/modules-load.d/uinput.conf
sudo modprobe uinput

# Cài binary, config và XKB keymap
sudo install -Dm755 target/release/vi-uinput-ime-vni /usr/local/bin/vi-uinput-ime-vni
sudo install -Dm644 config/default_config.toml /etc/vi-uinput-ime-vni/config.toml
sudo install -Dm644 config/vn_virtual.xkb /etc/vi-uinput-ime/vn_virtual.xkb

# Cài udev rule
sudo install -Dm644 deploy/99-vi-uinput-ime.rules /etc/udev/rules.d/99-vi-uinput-ime.rules
sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=misc --subsystem-match=input

# Cài và bật systemd service (tự động xung đột an toàn với vi-uinput-ime-telex.service)
sudo install -Dm644 deploy/vi-uinput-ime-vni.service /etc/systemd/system/vi-uinput-ime-vni.service
sudo systemctl daemon-reload
sudo systemctl disable --now vi-uinput-ime-telex.service 2>/dev/null || true
sudo systemctl enable --now vi-uinput-ime-vni.service

# Cài cầu nối đồng bộ Layout Desktop & Compositor IPC (BẮT BUỘC cho tự động tạm dừng khi chuyển sang US/IBus/Fcitx5 trên GNOME/KDE)
install -Dm644 deploy/vi-uinput-ime-vni-ipc.service ~/.config/systemd/user/vi-uinput-ime-vni-ipc.service
systemctl --user daemon-reload
systemctl --user enable --now vi-uinput-ime-vni-ipc.service
```

## 3. Kích hoạt layout XKB 134 ký tự (`vi_uinput`, miễn nhiễm CapsLock ở Level 5–8)

### GNOME / Mutter Wayland (song song với US / IBus / Fcitx5)
```bash
/usr/local/bin/vi-uinput-ime-vni --install-gnome-xkb
gsettings set org.gnome.desktop.input-sources show-all-sources true
# Khai báo cả US và vi_uinput; khi chuyển sang US/IBus daemon tự tạm dừng về PassThrough:
gsettings set org.gnome.desktop.input-sources sources "[('xkb', 'us'), ('xkb', 'vi_uinput')]"
# Khuyến nghị giữ nguồn nhập dùng chung cho mọi cửa sổ để mru-sources luôn đồng bộ:
gsettings set org.gnome.desktop.input-sources per-window false
```

### KDE Plasma / KWin Wayland (song song với US / Fcitx5)
```bash
/usr/local/bin/vi-uinput-ime-vni --install-gnome-xkb
kwriteconfig6 --file kxkbrc --group Layout --key Use true
kwriteconfig6 --file kxkbrc --group Layout --key LayoutList us,vi_uinput
```

### KDE Plasma X11 (Xorg — hỗ trợ đồng thời cài native `/usr/share/X11/xkb` & tự phục hồi `xkbcomp`)
```bash
# Cài native vào /usr/share/X11/xkb (symbols/vi_uinput + types/vi_uinput + patch types/complete & rules/evdev.{xml,lst})
sudo /usr/local/bin/vi-uinput-ime-vni --install-x11-xkb
# (Gỡ sạch mọi thay đổi trong /usr/share/X11/xkb bất cứ lúc nào bằng: sudo /usr/local/bin/vi-uinput-ime-vni --uninstall-x11-xkb)

# Đảm bảo user-session IPC bridge đang chạy để tự phục hồi keymap qua xkbcomp khi Xorg reset Virtual Core Keyboard / hotplug uinput
systemctl --user restart vi-uinput-ime-vni-ipc.service
```

### Hyprland / Sway / X11 thuần
- **Hyprland:** `device { name = vi-uinput-virtual-kbd; kb_file = /etc/vi-uinput-ime/vn_virtual.xkb; }` *(có thể đặt `toggle = "ctrl+space"` trong `config.toml` nếu không dùng bộ chuyển layout của GNOME/KDE)*
- **Sway:** `input "4617:1:vi-uinput-virtual-kbd" { xkb_file /etc/vi-uinput-ime/vn_virtual.xkb }`
- **X11 thuần (không có KDE):** dùng `--install-x11-xkb` hoặc để `--ipc-bridge` tự nạp/phục hồi bằng `xkbcomp`.
