# pinterest-wallpaper

Интерактивные обои для Hyprland: masonry-лента Pinterest на `zwlr_layer_shell`. Колёсико по пустому столу листает ленту, клик по пину открывает ссылку.

Wallpin читает уже скачанные файлы с диска. Здесь картинки берутся из сети и кладутся в `~/.cache/pinterest-wallpaper/` (лимит ~500 МБ).

## Источники

| `feed` в конфиге | Откуда | Логин |
|---|---|---|
| `homefeed` | домашняя лента (неофициальный web API) | `login-session` |
| `all_boards` | все свои доски | `login-oauth` |
| `{ board = { id = "…" } }` | одна доска | `login-oauth` |
| `{ local = { path = "…" } }` | папка с картинками | не нужен |

Официальный API Pinterest **не отдаёт home feed** — только доски и пины. Домашняя лента идёт через cookie `_auth` / `csrftoken` и может сломаться, если Pinterest поменяет фронт.

## Сборка (Arch)

```bash
sudo pacman -S gtk4 gtk4-layer-shell gdk-pixbuf2 pkgconf rust
cargo build --release
```

Бинарник: `target/release/pinterest-wallpaper`.

## Конфиг

`~/.config/pinterest-wallpaper/config.toml` создаётся при первом запуске. Пример — [`config.example.toml`](config.example.toml).

OAuth (доски): приложение на [developers.pinterest.com](https://developers.pinterest.com), redirect `http://127.0.0.1:8765/callback`, секция `[oauth]` в конфиге.

```bash
pinterest-wallpaper login-oauth     # браузер, токены в auth.json (0600)
pinterest-wallpaper login-session   # вставить _auth и csrftoken
pinterest-wallpaper logout
pinterest-wallpaper                 # показать обои
```

## Hyprland

Не запускай `hyprpaper` на тех же мониторах — два wallpaper на layer-shell конфликтуют. Убери или закомментируй его `exec-once`.

```conf
# ~/.config/hypr/hyprland.conf
exec-once = /path/to/pinterest-wallpaper
```

Скролл приходит на слой `background`, только когда курсор над пустым столом, не над окном. Если колёсико не листает ленту, поставь в конфиге `layer = "bottom"`.

Клавиатуру обои не забирают (`keyboard_mode = none`).

## Приватность

Токены и кэш остаются на машине. Текст для формы Pinterest: [PRIVACY.md](PRIVACY.md).
