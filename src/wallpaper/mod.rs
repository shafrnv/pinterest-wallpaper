mod masonry;
mod menu;

use anyhow::{Context, Result};
use gdk4::prelude::*;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::sync::Arc;
use tracing::{error, info};

use crate::cache::ImageCache;
use crate::config::Config;
use crate::pinterest::FeedManager;
use masonry::{Canvas, MonitorSlot};

pub fn run(cfg: Config) -> Result<()> {
    let app = Application::builder()
        .application_id("dev.pinterest.wallpaper")
        .build();

    let cfg = Arc::new(cfg);

    app.connect_activate(move |app| {
        if let Err(e) = activate(app, cfg.clone()) {
            error!("failed to start wallpaper: {e:#}");
            eprintln!("error: {e:#}");
            app.quit();
        }
    });

    let code = app.run_with_args::<&str>(&[]);
    if code != glib::ExitCode::SUCCESS {
        anyhow::bail!("gtk exited with {code:?}");
    }
    Ok(())
}

fn activate(app: &Application, cfg: Arc<Config>) -> Result<()> {
    apply_css(&cfg.background);

    let display = gdk4::Display::default().context("no display")?;
    let monitors = display.monitors();
    let n = monitors.n_items();
    if n == 0 {
        anyhow::bail!("no monitors");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let runtime = Arc::new(runtime);

    let cache = Arc::new(ImageCache::new()?);
    let feed = runtime.block_on(FeedManager::from_config(&cfg))?;
    let feed = Arc::new(tokio::sync::Mutex::new(feed));

    if !gtk4_layer_shell::is_supported() {
        anyhow::bail!("compositor does not support wlr-layer-shell");
    }

    let mut found = Vec::new();
    for i in 0..n {
        let Some(obj) = monitors.item(i) else {
            continue;
        };
        let monitor = obj
            .downcast::<gdk4::Monitor>()
            .map_err(|_| anyhow::anyhow!("monitor downcast"))?;
        let connector = monitor
            .connector()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("monitor-{i}"));
        let geometry = monitor.geometry();
        found.push(LiveMonitor {
            name: connector,
            monitor,
            width: geometry.width().max(1),
            height: geometry.height().max(1),
            compositor_x: geometry.x(),
            compositor_y: geometry.y(),
        });
    }

    let slots = layout_slots(&cfg, &found);
    if slots.is_empty() {
        anyhow::bail!("no monitors match the canvas layout in config.toml");
    }
    for slot in &slots {
        info!(
            "canvas {} at {},{} size {}x{}",
            slot.name, slot.x, slot.y, slot.width, slot.height
        );
    }
    let canvas = Canvas::new(cfg.clone(), cache, feed, runtime, slots.clone());

    for live in &found {
        let Some(view) = canvas.widget(&live.name) else {
            continue;
        };
        let win = ApplicationWindow::builder()
            .application(app)
            .title(format!("pinterest-wallpaper:{}", live.name))
            .decorated(false)
            .build();

        win.init_layer_shell();
        let layer = match cfg.layer.as_str() {
            "bottom" => Layer::Bottom,
            _ => Layer::Background,
        };
        win.set_layer(layer);
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Bottom, true);
        win.set_anchor(Edge::Left, true);
        win.set_anchor(Edge::Right, true);
        // -1: cover the whole output, including the strip under waybar.
        // Hyprland does not reserve space for a negative zone, so the bar stays on top.
        win.set_exclusive_zone(-1);
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_namespace("pinterest-wallpaper");
        win.set_monitor(&live.monitor);
        win.set_default_size(live.width, live.height);
        win.set_child(Some(view));
        win.present();
    }

    Ok(())
}

struct LiveMonitor {
    name: String,
    monitor: gdk4::Monitor,
    width: i32,
    height: i32,
    compositor_x: i32,
    compositor_y: i32,
}

/// Config positions win. Missing entries fall back to where Hyprland put the output.
fn layout_slots(cfg: &Config, found: &[LiveMonitor]) -> Vec<MonitorSlot> {
    if cfg.monitors.is_empty() {
        return found
            .iter()
            .map(|m| MonitorSlot {
                name: m.name.clone(),
                x: m.compositor_x,
                y: m.compositor_y,
                width: m.width,
                height: m.height,
            })
            .collect();
    }

    let mut slots = Vec::new();
    for placement in &cfg.monitors {
        let Some(live) = found.iter().find(|m| m.name == placement.name) else {
            tracing::warn!("monitor {} from config is not connected", placement.name);
            continue;
        };
        slots.push(MonitorSlot {
            name: live.name.clone(),
            x: placement.x,
            y: placement.y,
            width: live.width,
            height: live.height,
        });
    }
    slots
}

fn apply_css(bg: &str) {
    let css = format!(
        r#"
        window {{
            background-color: {bg};
        }}
        .pin-tile {{
            border-radius: 12px;
            background-color: #1a1a1a;
        }}
        scrolledwindow {{
            background-color: {bg};
        }}
        scrolledwindow.pin-canvas scrollbar,
        scrolledwindow.pin-canvas scrollbar slider,
        scrolledwindow.pin-canvas undershoot,
        scrolledwindow.pin-canvas overshoot {{
            opacity: 0;
            background: none;
            background-image: none;
            border: none;
            box-shadow: none;
        }}
        .autoscroll-mark {{
            color: #ffffff;
            background-color: rgba(20, 20, 20, 0.78);
            border-radius: 14px;
            padding: 1px 7px 3px;
            font-size: 18px;
            font-weight: bold;
        }}
        popover.pin-save,
        popover.pin-note {{
            background: transparent;
            border: none;
            box-shadow: none;
            padding: 0;
        }}
        popover.pin-save > contents,
        popover.pin-note > contents {{
            background-color: #111111;
            color: #ffffff;
            border: 1px solid rgba(255, 255, 255, 0.14);
            outline: none;
            box-shadow: none;
            border-radius: 26px;
            padding: 6px;
        }}
        popover.pin-note > contents {{
            border-radius: 16px;
            padding: 10px 14px;
        }}
        popover.pin-save > arrow,
        popover.pin-note > arrow {{
            opacity: 0;
            min-width: 0;
            min-height: 0;
            background: none;
            border: none;
        }}
        popover.pin-save label,
        popover.pin-note label {{
            color: #ffffff;
        }}
        popover.pin-save label.pin-title {{
            font-size: 16px;
            font-weight: 700;
        }}
        popover.pin-save label.pin-section {{
            color: rgba(255, 255, 255, 0.5);
            font-size: 13px;
            font-weight: 500;
        }}
        popover.pin-save scrolledwindow,
        popover.pin-save viewport,
        popover.pin-save stack,
        popover.pin-save box {{
            background-color: transparent;
            border: none;
            box-shadow: none;
        }}
        popover.pin-save .pin-rule {{
            background-color: rgba(255, 255, 255, 0.14);
            min-width: 1px;
        }}
        popover.pin-save button.pin-rail-btn {{
            background: transparent;
            color: #ffffff;
            border: none;
            outline: none;
            box-shadow: none;
            border-radius: 14px;
            padding: 0;
            min-width: 46px;
            min-height: 46px;
            margin: 2px;
        }}
        popover.pin-save button.pin-rail-btn.on,
        popover.pin-save button.pin-rail-btn:hover {{
            background: rgba(255, 255, 255, 0.14);
        }}
        popover.pin-save label.pin-rail-glyph {{
            color: #ffffff;
            font-size: 18px;
            font-weight: 600;
        }}
        popover.pin-save button.pin-like.liked label {{
            color: #e60023;
        }}
        popover.pin-save label.pin-status {{
            color: #ffd0d0;
            font-size: 13px;
        }}
        popover.pin-save button.pin-choice {{
            background: transparent;
            border: none;
            box-shadow: none;
            padding: 1px 2px;
            min-height: 0;
            border-radius: 8px;
        }}
        popover.pin-save button.pin-choice:hover {{
            background: rgba(255, 255, 255, 0.06);
        }}
        popover.pin-save .pin-dot {{
            border-radius: 999px;
            border: 1.5px solid #ffffff;
            background: transparent;
            min-width: 14px;
            min-height: 14px;
        }}
        popover.pin-save .pin-dot.on {{
            background: #ffffff;
            box-shadow: inset 0 0 0 3px #111111;
        }}
        popover.pin-save scrolledwindow.pin-people scrollbar {{
            background: transparent;
            border: none;
            min-width: 10px;
        }}
        popover.pin-save scrolledwindow.pin-people scrollbar slider {{
            background: rgba(255, 255, 255, 0.55);
            border-radius: 4px;
            min-width: 6px;
            min-height: 24px;
        }}
        popover.pin-save button.pin-share {{
            border-radius: 20px;
            min-width: 40px;
            min-height: 40px;
            padding: 0;
            background: #2a2a2a;
            color: #ffffff;
            border: none;
            box-shadow: none;
            font-weight: 700;
        }}
        popover.pin-save button.pin-share.fb {{ background: #1877f2; }}
        popover.pin-save button.pin-share.x {{
            background: #f4f1e8;
            color: #111111;
        }}
        popover.pin-save button.pin-size {{
            background: transparent;
            color: #ffffff;
            border: none;
            border-radius: 16px;
            padding: 6px 10px;
        }}
        popover.pin-save button.pin-size.on {{
            background: #f2f2f2;
            color: #111111;
        }}
        popover.pin-note button.pin-undo {{
            background: #2a2a2a;
            color: #ffffff;
            border: none;
            border-radius: 14px;
            padding: 4px 10px;
        }}
        popover.pin-save entry.pin-search {{
            background: transparent;
            color: #ffffff;
            caret-color: #ffffff;
            border: 1.5px solid rgba(255, 255, 255, 0.28);
            border-radius: 18px;
            outline: none;
            box-shadow: none;
            min-height: 42px;
            padding: 0 10px;
            margin-top: 2px;
        }}
        popover.pin-save entry.pin-search text {{
            background: transparent;
            color: #ffffff;
        }}
        popover.pin-save entry.pin-search image {{
            color: rgba(255, 255, 255, 0.72);
        }}
        popover.pin-save entry.pin-search placeholder,
        popover.pin-save entry.pin-search text placeholder {{
            color: rgba(255, 255, 255, 0.45);
        }}
        popover.pin-save button.pin-board,
        popover.pin-save button.pin-create {{
            background: transparent;
            color: #ffffff;
            border: none;
            outline: none;
            box-shadow: none;
            border-radius: 12px;
            padding: 4px 6px;
            min-height: 0;
        }}
        popover.pin-save button.pin-board:hover,
        popover.pin-save button.pin-create:hover {{
            background: rgba(255, 255, 255, 0.07);
        }}
        popover.pin-save button:focus,
        popover.pin-save entry:focus {{
            outline: none;
            box-shadow: none;
        }}
        popover.pin-save button.pin-board label {{
            color: #ffffff;
            font-size: 15px;
            font-weight: 600;
        }}
        popover.pin-save .pin-cover {{
            background-color: #222222;
            border-radius: 10px;
        }}
        popover.pin-save .pin-tick {{
            background-color: rgba(255, 255, 255, 0.88);
            border-radius: 2px;
            min-height: 3px;
            min-width: 36px;
        }}
        popover.pin-save .pin-plus {{
            background-color: #2a2a2a;
            border-radius: 10px;
            min-width: 36px;
            min-height: 36px;
        }}
        popover.pin-save .pin-plus label {{
            color: #ffffff;
            font-size: 22px;
            font-weight: 700;
        }}
        popover.pin-save button.pin-create label.pin-create-name {{
            color: #ffffff;
            font-size: 15px;
            font-weight: 700;
        }}
        popover.pin-save entry.pin-create-entry {{
            background: transparent;
            color: #ffffff;
            caret-color: #ffffff;
            border: none;
            outline: none;
            box-shadow: none;
            min-height: 0;
            padding: 0;
        }}
        popover.pin-save entry.pin-create-entry text {{
            background: transparent;
            color: #ffffff;
        }}
        popover.pin-save entry.pin-create-entry placeholder,
        popover.pin-save entry.pin-create-entry text placeholder {{
            color: rgba(255, 255, 255, 0.45);
        }}
        popover.pin-save scrollbar {{
            background: transparent;
            border: none;
            box-shadow: none;
        }}
        popover.pin-save scrollbar slider {{
            background: rgba(255, 255, 255, 0.28);
            border-radius: 4px;
            min-width: 4px;
        }}
        "#
    );
    let provider = CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gdk4::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
