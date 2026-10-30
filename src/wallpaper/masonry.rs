use gtk4::prelude::*;
use gtk4::{
    Adjustment, Box as GtkBox, Button, Orientation, Picture, ScrolledWindow, Widget,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::cache::ImageCache;
use crate::config::Config;
use crate::pinterest::{FeedManager, PinItem};

pub struct MasonryFeed {
    scroll: ScrolledWindow,
}

impl MasonryFeed {
    pub fn new(
        cfg: Arc<Config>,
        cache: Arc<ImageCache>,
        feed: Arc<tokio::sync::Mutex<FeedManager>>,
        runtime: Arc<tokio::runtime::Runtime>,
        viewport_width: i32,
    ) -> Self {
        let columns = cfg.columns.max(1) as usize;
        let gap = cfg.gap.max(0);

        let hbox = GtkBox::new(Orientation::Horizontal, gap);
        hbox.set_margin_start(gap);
        hbox.set_margin_end(gap);
        hbox.set_margin_top(gap);
        hbox.set_margin_bottom(gap);
        hbox.set_hexpand(true);
        hbox.set_halign(gtk4::Align::Fill);

        let col_width = ((viewport_width - gap * (columns as i32 + 1)).max(120)) / columns as i32;

        let mut cols: Vec<GtkBox> = Vec::with_capacity(columns);
        for _ in 0..columns {
            let col = GtkBox::new(Orientation::Vertical, gap);
            col.set_width_request(col_width);
            col.set_hexpand(false);
            hbox.append(&col);
            cols.push(col);
        }

        let scroll = ScrolledWindow::builder()
            .child(&hbox)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .hexpand(true)
            .vexpand(true)
            .build();

        let heights = Rc::new(RefCell::new(vec![0i32; columns]));
        let loading = Rc::new(RefCell::new(false));

        let cols = Rc::new(cols);
        let heights_c = heights.clone();
        let loading_c = loading.clone();
        let scroll_c = scroll.clone();
        let cache_c = cache.clone();
        let feed_c = feed.clone();
        let runtime_c = runtime.clone();
        let col_width_c = col_width;

        let load_more: Rc<dyn Fn()> = Rc::new(|| {});
        // placeholder replaced below — we need a self-referential callback
        drop(load_more);

        let load_more: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let load_more_slot = load_more.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            if *loading_c.borrow() {
                return;
            }
            *loading_c.borrow_mut() = true;

            let feed = feed_c.clone();
            let runtime = runtime_c.clone();
            let cache = cache_c.clone();
            let cols = cols.clone();
            let heights = heights_c.clone();
            let loading = loading_c.clone();
            let scroll = scroll_c.clone();
            let slot = load_more_slot.clone();
            let col_w = col_width_c;
            let gap = gap;

            glib::spawn_future_local(async move {
                let result = runtime
                    .spawn(async move {
                        let mut mgr = feed.lock().await;
                        mgr.next_page().await
                    })
                    .await;

                let pins = match result {
                    Ok(Ok(pins)) => pins,
                    Ok(Err(e)) => {
                        warn!("feed page failed: {e:#}");
                        *loading.borrow_mut() = false;
                        return;
                    }
                    Err(e) => {
                        warn!("feed task failed: {e}");
                        *loading.borrow_mut() = false;
                        return;
                    }
                };

                if pins.is_empty() {
                    debug!("feed exhausted");
                    *loading.borrow_mut() = false;
                    return;
                }

                for pin in pins {
                    let local = match resolve_image(&runtime, &cache, &pin).await {
                        Some(p) => p,
                        None => continue,
                    };

                    let aspect = if pin.width == 0 {
                        1.3
                    } else {
                        pin.height as f64 / pin.width as f64
                    };
                    // Local fixtures don't have real dimensions — probe pixbuf size later via picture
                    let tile_h = ((col_w as f64) * aspect).round() as i32;
                    let tile_h = tile_h.clamp(80, 1600);

                    let idx = shortest(&heights.borrow());
                    let picture = Picture::for_filename(&local);
                    picture.set_can_shrink(true);
                    picture.set_width_request(col_w);
                    picture.set_height_request(tile_h);
                    picture.set_content_fit(gtk4::ContentFit::Cover);
                    picture.add_css_class("pin-tile");

                    let widget: Widget = if let Some(link) = pin.link.clone() {
                        let btn = Button::new();
                        btn.set_child(Some(&picture));
                        btn.add_css_class("pin-tile");
                        btn.set_has_frame(false);
                        btn.connect_clicked(move |_| {
                            let _ = open::that(&link);
                        });
                        btn.upcast()
                    } else {
                        picture.upcast()
                    };

                    cols[idx].append(&widget);
                    heights.borrow_mut()[idx] += tile_h + gap;
                }

                *loading.borrow_mut() = false;

                // If content still doesn't fill the viewport, keep loading
                {
                    let adj = scroll.vadjustment();
                    if adj.upper() <= adj.page_size() + 40.0 {
                        if let Some(next) = slot.borrow().clone() {
                            next();
                        }
                    }
                }
            });
        });

        *load_more.borrow_mut() = Some(cb.clone());

        let adj = scroll.vadjustment();
        let cb_scroll = cb.clone();
        let loading_s = loading.clone();
        adj.connect_value_changed(move |adj: &Adjustment| {
            if *loading_s.borrow() {
                return;
            }
            let near_end = adj.value() + adj.page_size() >= adj.upper() - 400.0;
            if near_end {
                cb_scroll();
            }
        });

        // Initial fill
        cb();

        Self { scroll }
    }

    pub fn widget(&self) -> &ScrolledWindow {
        &self.scroll
    }
}

fn shortest(heights: &[i32]) -> usize {
    heights
        .iter()
        .enumerate()
        .min_by_key(|(_, h)| *h)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

async fn resolve_image(
    runtime: &tokio::runtime::Runtime,
    cache: &ImageCache,
    pin: &PinItem,
) -> Option<std::path::PathBuf> {
    if let Some(path) = pin.image_url.strip_prefix("file://") {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    let url = pin.image_url.clone();
    let cache = cache.clone();
    match runtime
        .spawn(async move { cache.fetch(&url).await })
        .await
    {
        Ok(Ok(p)) => Some(p),
        Ok(Err(e)) => {
            warn!("image {}: {e:#}", pin.id);
            None
        }
        Err(e) => {
            warn!("image task {}: {e}", pin.id);
            None
        }
    }
}
