mod masonry;

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
use masonry::MasonryFeed;

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

    let code = app.run();
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
        info!("creating wallpaper on {connector}");

        let feed = runtime.block_on(FeedManager::from_config(&cfg))?;
        let feed = Arc::new(tokio::sync::Mutex::new(feed));

        let win = ApplicationWindow::builder()
            .application(app)
            .title(format!("pinterest-wallpaper:{connector}"))
            .decorated(false)
            .build();

        if !gtk4_layer_shell::is_supported() {
            anyhow::bail!("compositor does not support wlr-layer-shell");
        }

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
        // 0 = not a panel; windows cover the wallpaper normally.
        win.set_exclusive_zone(0);
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_namespace("pinterest-wallpaper");
        win.set_monitor(&monitor);

        let geometry = monitor.geometry();
        let width = geometry.width().max(1);
        let height = geometry.height().max(1);
        win.set_default_size(width, height);

        let masonry = MasonryFeed::new(
            cfg.clone(),
            cache.clone(),
            feed.clone(),
            runtime.clone(),
            width,
        );
        win.set_child(Some(masonry.widget()));
        win.present();
    }

    Ok(())
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
        .pin-tile:hover {{
            opacity: 0.92;
        }}
        scrolledwindow {{
            background-color: {bg};
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
