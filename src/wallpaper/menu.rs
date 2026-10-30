use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, DrawingArea, Entry, EntryIconPosition, Label, Orientation,
    Overflow, Picture, Popover, ScrolledWindow, Widget,
};
use gtk4_layer_shell::{KeyboardMode, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::ImageCache;
use crate::pinterest::actions::{self, BoardDest, SaveCatalog};

/// Right-click opens the same save sheet Pinterest shows: boards, search, create.
pub struct PinMenu {
    runtime: Arc<tokio::runtime::Runtime>,
    cache: Arc<ImageCache>,
    scrolls: Vec<ScrolledWindow>,
    sheets: Vec<Popover>,
    notes: Vec<Popover>,
    note_labels: Vec<Label>,
    catalog: RefCell<SaveCatalog>,
    busy: Cell<bool>,
    last_monitor: Cell<usize>,
    open_pin: RefCell<Option<String>>,
    open_image: RefCell<Option<std::path::PathBuf>>,
    open_aspect: Cell<f64>,
    liked: RefCell<std::collections::HashSet<String>>,
    status: RefCell<Option<Label>>,
    undo_buttons: Vec<Button>,
    undo_cb: RefCell<Option<Rc<dyn Fn()>>>,
    list_gen: Cell<u64>,
    /// How many text fields in the sheet currently hold the keyboard.
    typing: Rc<Cell<i32>>,
    /// Keeps the sheet up while a page change resizes the popup.
    hold: Rc<SheetHold>,
    /// Hyprland window that was focused when the sheet opened.
    focus_at_open: RefCell<String>,
    opened_ms: Cell<u128>,
}

struct SheetHold {
    open: Cell<bool>,
    gen: Cell<u32>,
}

impl PinMenu {
    pub fn new(
        runtime: Arc<tokio::runtime::Runtime>,
        cache: Arc<ImageCache>,
        scrolls: &[ScrolledWindow],
        typing: Rc<Cell<i32>>,
    ) -> Rc<Self> {
        let mut sheets = Vec::new();
        let mut notes = Vec::new();
        let mut note_labels = Vec::new();
        let mut undo_buttons = Vec::new();
        let hold = Rc::new(SheetHold {
            open: Cell::new(false),
            gen: Cell::new(0),
        });
        for scroll in scrolls {
            let sheet = Popover::new();
            sheet.set_parent(scroll);
            sheet.set_has_arrow(false);
            // No input grab: a wheel notch or a click on a window must reach
            // the desktop, and the sheet closes from that same event.
            sheet.set_autohide(false);
            sheet.add_css_class("pin-save");
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_max_width_chars(28);
            let note = Popover::new();
            note.set_parent(scroll);
            note.set_has_arrow(false);
            note.set_autohide(true);
            note.add_css_class("pin-note");
            let undo = Button::with_label("отмена");
            undo.add_css_class("pin-undo");
            undo.set_visible(false);
            let row = Box::new(Orientation::Horizontal, 12);
            row.append(&label);
            row.append(&undo);
            note.set_child(Some(&row));
            let scroll_keys = scroll.clone();
            let note_keys = note.clone();
            let typing_closed = typing.clone();
            let hold_closed = hold.clone();
            let sheet_reopen = sheet.clone();
            sheet.connect_closed(move |_| {
                // A page switch resizes the Wayland popup, and the compositor
                // answers with popup_done. Put the same sheet back.
                if hold_closed.open.get() {
                    let sheet = sheet_reopen.clone();
                    let hold = hold_closed.clone();
                    let gen = hold.gen.get();
                    glib::idle_add_local_once(move || {
                        if hold.gen.get() == gen && hold.open.get() {
                            sheet.popup();
                        }
                    });
                    return;
                }
                set_typing(&scroll_keys, false);
                typing_closed.set(0);
                if note_keys.is_visible() {
                    note_keys.popdown();
                }
            });
            sheets.push(sheet);
            notes.push(note);
            note_labels.push(label);
            undo_buttons.push(undo);
        }

        let menu = Rc::new(Self {
            runtime,
            cache,
            scrolls: scrolls.to_vec(),
            sheets,
            notes,
            note_labels,
            catalog: RefCell::new(SaveCatalog::default()),
            busy: Cell::new(false),
            last_monitor: Cell::new(0),
            open_pin: RefCell::new(None),
            open_image: RefCell::new(None),
            open_aspect: Cell::new(1.4),
            liked: RefCell::new(std::collections::HashSet::new()),
            status: RefCell::new(None),
            undo_buttons,
            undo_cb: RefCell::new(None),
            list_gen: Cell::new(0),
            typing,
            hold,
            focus_at_open: RefCell::new(String::new()),
            opened_ms: Cell::new(0),
        });
        for undo in &menu.undo_buttons {
            let menu_undo = menu.clone();
            undo.connect_clicked(move |_| {
                if let Some(cb) = menu_undo.undo_cb.borrow_mut().take() {
                    cb();
                }
                let monitor = menu_undo.last_monitor.get();
                if let Some(note) = menu_undo.notes.get(monitor) {
                    note.popdown();
                }
            });
        }
        let refresh = menu.clone();
        glib::spawn_future_local(async move {
            refresh.reload_catalog().await;
        });
        menu
    }

    pub fn attach(
        self: &Rc<Self>,
        picture: &gtk4::Picture,
        monitor: usize,
        pin_id: &str,
        link: Option<&str>,
        image: &std::path::Path,
        aspect: f64,
    ) {
        let click = gtk4::GestureClick::new();
        click.set_button(3);
        let menu = self.clone();
        let picture_for_point = picture.clone();
        let pin_id = pin_id.to_string();
        let link = link.map(|s| s.to_string());
        let image = image.to_path_buf();
        click.connect_pressed(move |_, _, x, y| {
            menu.popup(
                monitor,
                &picture_for_point,
                x,
                y,
                &pin_id,
                link.as_deref(),
                &image,
                aspect,
            );
        });
        picture.add_controller(click);
    }

    fn popup(
        self: &Rc<Self>,
        monitor: usize,
        picture: &gtk4::Picture,
        x: f64,
        y: f64,
        pin_id: &str,
        link: Option<&str>,
        image: &std::path::Path,
        aspect: f64,
    ) {
        let Some(scroll) = self.scrolls.get(monitor) else {
            return;
        };
        let Some(sheet) = self.sheets.get(monitor) else {
            return;
        };
        self.last_monitor.set(monitor);
        *self.open_pin.borrow_mut() = Some(pin_id.to_string());
        *self.open_image.borrow_mut() = Some(image.to_path_buf());
        self.open_aspect.set(if aspect.is_finite() && aspect > 0.2 {
            aspect
        } else {
            1.4
        });
        let _ = link;
        let point = picture
            .compute_point(scroll, &gtk4::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk4::graphene::Point::new(x as f32, y as f32));
        let rect = gdk4::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1);
        sheet.set_child(Some(&self.build_sheet(pin_id)));
        sheet.set_pointing_to(Some(&rect));
        if let Some(note) = self.notes.get(monitor) {
            note.popdown();
        }
        set_typing(scroll, true);
        self.focus_at_open.replace(current_window_address());
        self.opened_ms.set(unix_ms());
        sheet.popup();
        if let Some(entry) = sheet
            .child()
            .as_ref()
            .and_then(|child| find_search(child))
        {
            let entry = entry.clone();
            glib::idle_add_local_once(move || {
                entry.grab_focus();
            });
        }
    }

    fn build_sheet(self: &Rc<Self>, pin_id: &str) -> Box {
        *self.status.borrow_mut() = None;
        let root = Box::new(Orientation::Horizontal, 0);

        if !actions::is_pinterest_pin(pin_id) {
            let label = Label::new(Some("Локальная картинка"));
            label.add_css_class("pin-title");
            label.set_margin_top(12);
            label.set_margin_bottom(12);
            label.set_margin_start(16);
            label.set_margin_end(16);
            root.append(&label);
            return root;
        }

        let rail = Box::new(Orientation::Vertical, 2);
        rail.add_css_class("pin-rail");
        rail.set_margin_top(6);
        rail.set_margin_bottom(6);
        rail.set_margin_start(6);

        let pages = gtk4::Stack::new();
        pages.add_css_class("pin-pages");
        pages.set_hhomogeneous(false);
        pages.set_vhomogeneous(false);
        pages.set_hexpand(true);
        pages.set_margin_start(8);
        pages.set_margin_end(4);
        pages.set_margin_top(8);
        pages.set_margin_bottom(8);

        pages.add_named(&self.comments_page(pin_id), Some("comments"));
        pages.add_named(&self.share_page(pin_id), Some("share"));
        pages.add_named(&self.more_page(pin_id, &pages), Some("more"));
        pages.add_named(&self.collage_page(pin_id, &pages), Some("collage"));
        pages.add_named(&self.less_page(pin_id, &pages), Some("less"));
        pages.add_named(&self.report_page(pin_id, &pages), Some("report"));
        pages.add_named(&self.embed_page(pin_id, &pages), Some("embed"));
        pages.add_named(&self.save_page(pin_id), Some("save"));
        pages.set_visible_child_name("save");

        let like = rail_button("♥", "Лайк");
        like.add_css_class("pin-like");
        if self.liked.borrow().contains(pin_id) {
            like.add_css_class("liked");
        }
        let menu_like = self.clone();
        let pin_like = pin_id.to_string();
        like.connect_clicked(move |button| {
            let turn_on = !button.has_css_class("liked");
            if turn_on {
                button.add_css_class("liked");
                menu_like.liked.borrow_mut().insert(pin_like.clone());
            } else {
                button.remove_css_class("liked");
                menu_like.liked.borrow_mut().remove(&pin_like);
            }
            let button = button.clone();
            let menu_like = menu_like.clone();
            let pin_like = pin_like.clone();
            let runtime = menu_like.runtime.clone();
            glib::spawn_future_local(async move {
                let pin = pin_like.clone();
                let result = runtime
                    .spawn(async move { actions::set_liked(&pin, turn_on).await })
                    .await;
                if let Err(err) = result.unwrap_or_else(|err| Err(err.into())) {
                    if turn_on {
                        button.remove_css_class("liked");
                        menu_like.liked.borrow_mut().remove(&pin_like);
                    } else {
                        button.add_css_class("liked");
                        menu_like.liked.borrow_mut().insert(pin_like.clone());
                    }
                    menu_like.flash(&err.to_string());
                    tracing::warn!("лайк: {}", clip_note(&err.to_string()));
                }
            });
        });
        rail.append(&like);

        let group: Rc<RefCell<Vec<Button>>> = Rc::new(RefCell::new(Vec::new()));
        for (glyph, tip, name) in [
            ("💬", "Комментарии", "comments"),
            ("⤴", "Поделиться", "share"),
            ("…", "Ещё", "more"),
            ("＋", "Сохранить", "save"),
        ] {
            let button = rail_button(glyph, tip);
            if name == "save" {
                button.add_css_class("on");
            }
            group.borrow_mut().push(button.clone());
            let pages = pages.clone();
            let group = group.clone();
            let name = name.to_string();
            let hold = self.hold.clone();
            button.connect_clicked(move |button| {
                show_page(&pages, &name, &hold);
                for other in group.borrow().iter() {
                    other.remove_css_class("on");
                }
                button.add_css_class("on");
            });
            rail.append(&button);
        }

        root.append(&rail);
        let rule = Box::new(Orientation::Vertical, 0);
        rule.add_css_class("pin-rule");
        rule.set_size_request(1, -1);
        root.append(&rule);
        let column = Box::new(Orientation::Vertical, 4);
        column.set_hexpand(true);
        let status = Label::new(None);
        status.add_css_class("pin-status");
        status.set_wrap(true);
        status.set_max_width_chars(32);
        status.set_halign(Align::Start);
        status.set_xalign(0.0);
        status.set_visible(false);
        column.append(&status);
        pages.set_vexpand(true);
        column.append(&pages);
        *self.status.borrow_mut() = Some(status);
        let frame = ScrolledWindow::new();
        frame.set_child(Some(&column));
        // Width follows the visible page. A fixed width clipped the longer
        // rows and slid them out to the right. Height stays put so the sheet
        // does not jump when the page changes.
        frame.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        frame.set_propagate_natural_width(false);
        frame.set_propagate_natural_height(true);
        frame.set_min_content_width(240);
        frame.set_max_content_width(420);
        frame.set_min_content_height(280);
        frame.set_max_content_height(460);
        frame.set_width_request(260);
        frame.set_height_request(420);
        frame.set_has_frame(false);
        frame.add_css_class("pin-sheet");
        frame.hscrollbar().set_visible(false);
        let frame_fit = frame.clone();
        let pages_fit = pages.clone();
        let hold_fit = self.hold.clone();
        let hold_map = self.hold.clone();
        pages.connect_visible_child_notify(move |pages| {
            let pages = pages.clone();
            let frame = frame_fit.clone();
            let hold = hold_fit.clone();
            glib::idle_add_local_once(move || fit_sheet_width(&frame, &pages, &hold));
        });
        frame.connect_map(move |frame| fit_sheet_width(frame, &pages_fit, &hold_map));
        root.append(&frame);
        root
    }

    fn save_page(self: &Rc<Self>, pin_id: &str) -> Box {
        let page = Box::new(Orientation::Vertical, 0);

        let title = Label::new(Some("сохранение"));
        title.add_css_class("pin-title");
        title.set_halign(Align::Center);
        title.set_margin_bottom(12);
        page.append(&title);

        let search = Entry::new();
        search.add_css_class("pin-search");
        search.set_placeholder_text(Some("поиск"));
        self.watch_entry(&search);
        search.set_icon_from_icon_name(EntryIconPosition::Primary, Some("edit-find-symbolic"));
        search.set_icon_activatable(EntryIconPosition::Primary, false);
        search.set_has_frame(false);
        search.set_hexpand(true);
        page.append(&search);

        let list = Box::new(Orientation::Vertical, 0);
        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_propagate_natural_width(true);
        scroll.set_max_content_height(360);
        scroll.set_has_frame(false);
        scroll.set_overlay_scrolling(false);
        scroll.set_margin_top(8);
        page.append(&scroll);

        let menu = self.clone();
        let pin = pin_id.to_string();
        let list_for_search = list.clone();
        search.connect_changed(move |entry| {
            menu.fill_boards(&list_for_search, &pin, &entry.text());
        });
        self.fill_boards(&list, pin_id, "");

        page.append(&self.create_row(pin_id));
        page
    }

    fn comments_page(self: &Rc<Self>, pin_id: &str) -> Box {
        let page = Box::new(Orientation::Vertical, 8);
        let title = Label::new(Some("комментарии"));
        title.add_css_class("pin-title");
        title.set_halign(Align::Center);
        page.append(&title);

        let list = Box::new(Orientation::Vertical, 12);
        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(320);
        scroll.set_has_frame(false);
        page.append(&scroll);

        let entry = Entry::new();
        entry.add_css_class("pin-search");
        entry.set_placeholder_text(Some("добавить комментарий"));
        self.watch_entry(&entry);
        page.append(&entry);

        let object_id = Rc::new(RefCell::new(pin_id.to_string()));
        let reply_to: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let menu = self.clone();
        let pin = pin_id.to_string();
        let list_fill = list.clone();
        let title_fill = title.clone();
        let object_fill = object_id.clone();
        let reply_fill = reply_to.clone();
        let entry_fill = entry.clone();
        glib::spawn_future_local(async move {
            let runtime = menu.runtime.clone();
            let pin = pin.clone();
            let loaded = runtime
                .spawn(async move { actions::load_comments(&pin).await })
                .await;
            match loaded {
                Ok(Ok((id, comments))) => {
                    *object_fill.borrow_mut() = id;
                    title_fill.set_text(&comments_title(comments.len()));
                    fill_comment_list(&list_fill, &comments, &reply_fill, &entry_fill);
                }
                Ok(Err(err)) => title_fill.set_text(&err.to_string()),
                Err(_) => title_fill.set_text("не удалось загрузить комментарии"),
            }
        });

        let menu = self.clone();
        let object_post = object_id.clone();
        let reply_post = reply_to.clone();
        entry.connect_activate(move |entry| {
            let text = entry.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            entry.set_sensitive(false);
            let menu = menu.clone();
            let object_id = object_post.borrow().clone();
            let parent = reply_post.borrow().clone();
            let entry = entry.clone();
            glib::spawn_future_local(async move {
                let runtime = menu.runtime.clone();
                let text_for_task = text.clone();
                let result = runtime
                    .spawn(async move {
                        actions::post_comment(&object_id, &text_for_task, parent.as_deref()).await
                    })
                    .await;
                entry.set_sensitive(true);
                match result {
                    Ok(Ok(())) => {
                        entry.set_text("");
                        menu.flash("комментарий отправлен");
                    }
                    Ok(Err(err)) => menu.flash(&err.to_string()),
                    Err(_) => menu.flash("не удалось отправить комментарий"),
                }
            });
        });
        page
    }

    fn share_page(self: &Rc<Self>, pin_id: &str) -> Box {
        let page = Box::new(Orientation::Vertical, 10);
        let title = Label::new(Some("общий доступ"));
        title.add_css_class("pin-title");
        title.set_halign(Align::Center);
        page.append(&title);

        let url = actions::pin_url(pin_id);
        let targets = Box::new(Orientation::Horizontal, 8);
        targets.set_halign(Align::Center);
        let col = Box::new(Orientation::Vertical, 4);
        col.set_halign(Align::Center);
        let button = Button::with_label("⧉");
        button.add_css_class("pin-share");
        button.add_css_class("copy");
        let cap = Label::new(Some("копировать\nссылку"));
        cap.add_css_class("pin-section");
        cap.set_justify(gtk4::Justification::Center);
        cap.set_halign(Align::Center);
        let url_copy = url.clone();
        let cap_copy = cap.clone();
        button.connect_clicked(move |button| {
            button.clipboard().set_text(&url_copy);
            cap_copy.set_text("скопировано");
        });
        col.append(&button);
        col.append(&cap);
        targets.append(&col);
        page.append(&targets);

        let search = Entry::new();
        search.add_css_class("pin-search");
        search.set_placeholder_text(Some("поиск людей"));
        self.watch_entry(&search);
        search.set_icon_from_icon_name(EntryIconPosition::Primary, Some("edit-find-symbolic"));
        search.set_icon_activatable(EntryIconPosition::Primary, false);
        search.set_has_frame(false);
        search.set_hexpand(true);
        page.append(&search);

        let list = Box::new(Orientation::Vertical, 2);
        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Always);
        scroll.set_propagate_natural_height(false);
        scroll.set_propagate_natural_width(false);
        scroll.set_vexpand(true);
        scroll.set_height_request(220);
        scroll.set_has_frame(false);
        scroll.set_overlay_scrolling(false);
        scroll.add_css_class("pin-people");
        page.append(&scroll);

        let menu = self.clone();
        let pin = pin_id.to_string();
        let gen = Rc::new(Cell::new(0u64));
        let load = {
            let menu = menu.clone();
            let pin = pin.clone();
            let list = list.clone();
            let gen = gen.clone();
            move |query: String| {
                let ticket = gen.get().wrapping_add(1);
                gen.set(ticket);
                let menu = menu.clone();
                let pin = pin.clone();
                let list = list.clone();
                let gen = gen.clone();
                glib::spawn_future_local(async move {
                    let runtime = menu.runtime.clone();
                    let query_task = query.clone();
                    let pin_task = pin.clone();
                    let loaded = runtime
                        .spawn(async move { actions::share_people(&pin_task, &query_task).await })
                        .await;
                    if gen.get() != ticket {
                        return;
                    }
                    while let Some(child) = list.first_child() {
                        list.remove(&child);
                    }
                    match loaded {
                        Ok(Ok(people)) if people.is_empty() => {
                            list.append(&section_label("никого не нашлось"));
                        }
                        Ok(Ok(people)) => {
                            for person in people {
                                list.append(&share_person_row(&menu, &pin, &person));
                            }
                        }
                        Ok(Err(err)) => list.append(&section_label(&err.to_string())),
                        Err(_) => list.append(&section_label("не удалось загрузить людей")),
                    }
                });
            }
        };
        load(String::new());
        search.connect_changed(move |entry| load(entry.text().to_string()));
        page
    }

    fn more_page(self: &Rc<Self>, pin_id: &str, pages: &gtk4::Stack) -> Box {
        let page = Box::new(Orientation::Vertical, 0);
        let pin = pin_id.to_string();
        let url = actions::pin_url(pin_id);
        page.append(&stack_button(&pages, "добавить в коллаж", "collage", &self.hold));
        let menu = self.clone();
        let pin_more = pin.clone();
        page.append(&action_button_cb("показать больше", move |_| {
            let menu = menu.clone();
            let pin_more = pin_more.clone();
            glib::spawn_future_local(async move {
                let runtime = menu.runtime.clone();
                let pin_task = pin_more.clone();
                let result = runtime
                    .spawn(async move { actions::pin_feedback(&pin_task, "see_more", None, false).await })
                    .await;
                match result {
                    Ok(Ok(())) => {
                        for sheet in &menu.sheets {
                            sheet.popdown();
                        }
                        let menu_undo = menu.clone();
                        let pin_undo = pin_more.clone();
                        menu.flash_undo(
                            "понятно! мы будем показывать вам больше таких пинов.",
                            Rc::new(move || {
                                let menu_undo = menu_undo.clone();
                                let pin_undo = pin_undo.clone();
                                glib::spawn_future_local(async move {
                                    let runtime = menu_undo.runtime.clone();
                                    let _ = runtime
                                        .spawn(async move {
                                            actions::pin_feedback(&pin_undo, "see_more", None, true).await
                                        })
                                        .await;
                                });
                            }),
                        );
                    }
                    Ok(Err(err)) => menu.flash(&err.to_string()),
                    Err(_) => menu.flash("не удалось"),
                }
            });
        }));
        page.append(&stack_button(&pages, "показать меньше", "less", &self.hold));
        page.append(&stack_button(&pages, "пожаловаться на пин", "report", &self.hold));
        page.append(&stack_button(
            &pages,
            "получить код для встраивания",
            "embed",
            &self.hold,
        ));
        let menu = self.clone();
        let pin_file = pin.clone();
        page.append(&action_button_cb("скачать изображение", move |_| {
            let path = menu.open_image.borrow().clone();
            let Some(path) = path else {
                menu.flash("файл картинки не найден");
                return;
            };
            match copy_download(&path, &pin_file) {
                Ok(dest) => menu.flash(&format!("сохранено в {dest}")),
                Err(err) => menu.flash(&err),
            }
        }));
        let url_open = url.clone();
        page.append(&action_button_cb("открыть пин", move |_| {
            let _ = open::that(&url_open);
        }));
        page
    }

    fn collage_page(self: &Rc<Self>, pin_id: &str, pages: &gtk4::Stack) -> Box {
        let page = Box::new(Orientation::Vertical, 8);
        page.append(&back_title(&pages, "добавить в коллаж", "more", &self.hold));
        let list = Box::new(Orientation::Vertical, 4);
        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(280);
        scroll.set_has_frame(false);
        page.append(&scroll);
        page.append(&self.collage_create_row(pin_id, &list));
        let menu = self.clone();
        let pin = pin_id.to_string();
        let list_fill = list.clone();
        glib::spawn_future_local(async move {
            let runtime = menu.runtime.clone();
            let loaded = runtime.spawn(actions::load_collages()).await;
            match loaded {
                Ok(Ok(items)) if items.is_empty() => {
                    list_fill.append(&section_label("коллажей пока нет"));
                }
                Ok(Ok(items)) => {
                    for item in items {
                        list_fill.append(&collage_row(&menu, &pin, &item));
                    }
                }
                Ok(Err(err)) => list_fill.append(&section_label(&err.to_string())),
                Err(_) => list_fill.append(&section_label("не удалось загрузить коллажи")),
            }
        });
        page
    }

    fn collage_create_row(self: &Rc<Self>, pin_id: &str, list: &Box) -> Box {
        let wrap = Box::new(Orientation::Horizontal, 8);
        let entry = Entry::new();
        entry.add_css_class("pin-search");
        entry.set_placeholder_text(Some("создать коллаж"));
        self.watch_entry(&entry);
        entry.set_hexpand(true);
        let menu = self.clone();
        let pin = pin_id.to_string();
        let list = list.clone();
        entry.connect_activate(move |entry| {
            let name = entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            entry.set_sensitive(false);
            let menu = menu.clone();
            let pin = pin.clone();
            let list = list.clone();
            let entry = entry.clone();
            glib::spawn_future_local(async move {
                let runtime = menu.runtime.clone();
                let name_task = name.clone();
                let pin_task = pin.clone();
                let result = runtime
                    .spawn(async move { actions::create_collage(&name_task, &pin_task).await })
                    .await;
                entry.set_sensitive(true);
                match result {
                    Ok(Ok(dest)) => {
                        entry.set_text("");
                        list.append(&collage_row(&menu, &pin, &dest));
                        menu.flash(&format!("коллаж «{}» создан", dest.name));
                    }
                    Ok(Err(err)) => menu.flash(&err.to_string()),
                    Err(_) => menu.flash("не удалось создать коллаж"),
                }
            });
        });
        wrap.append(&entry);
        wrap
    }

    fn less_page(self: &Rc<Self>, pin_id: &str, pages: &gtk4::Stack) -> Box {
        let page = Box::new(Orientation::Vertical, 4);
        page.append(&back_title(
            pages,
            "есть минутка? расскажите нам почему.",
            "more",
            &self.hold,
        ));
        for (label, reason) in [
            ("меня это не интересует", "not_interested"),
            ("пин на незнакомом мне языке", "language"),
            ("пин показывается слишком часто", "too_frequent"),
            ("размытое или мозаичное изображение", "blurry"),
            ("другое", "other"),
        ] {
            let menu_pin = pin_id.to_string();
            page.append(&feedback_button(self, &menu_pin, "see_less", reason, label));
        }
        page
    }

    fn report_page(self: &Rc<Self>, pin_id: &str, pages: &gtk4::Stack) -> Box {
        let page = Box::new(Orientation::Vertical, 6);
        page.append(&back_title(pages, "жалоба на пин", "more", &self.hold));
        let chosen: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let list = Box::new(Orientation::Vertical, 2);
        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&list));
        scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(320);
        scroll.set_has_frame(false);
        scroll.set_overlay_scrolling(false);
        let group: Rc<RefCell<Vec<(Button, Box)>>> = Rc::new(RefCell::new(Vec::new()));
        for (title, hint, key) in report_reasons() {
            list.append(&choice_row(title, hint, &group, &chosen, key));
        }
        page.append(&scroll);
        let actions = Box::new(Orientation::Horizontal, 8);
        actions.set_halign(Align::End);
        let cancel = Button::with_label("отмена");
        cancel.add_css_class("pin-board");
        let pages_back = pages.clone();
        let hold_back = self.hold.clone();
        cancel.connect_clicked(move |_| {
            show_page(&pages_back, "more", &hold_back);
        });
        let next = Button::with_label("далее");
        next.add_css_class("pin-board");
        let menu = self.clone();
        let pin = pin_id.to_string();
        next.connect_clicked(move |_| {
            let Some(reason) = chosen.borrow().clone() else {
                menu.flash("выберите причину");
                return;
            };
            let menu = menu.clone();
            let pin = pin.clone();
            glib::spawn_future_local(async move {
                let runtime = menu.runtime.clone();
                let result = runtime
                    .spawn(async move { actions::report_pin(&pin, &reason).await })
                    .await;
                match result {
                    Ok(Ok(())) => {
                        for sheet in &menu.sheets {
                            sheet.popdown();
                        }
                        menu.flash("жалоба отправлена");
                    }
                    Ok(Err(err)) => menu.flash(&err.to_string()),
                    Err(_) => menu.flash("не удалось отправить жалобу"),
                }
            });
        });
        actions.append(&cancel);
        actions.append(&next);
        page.append(&actions);
        page
    }

    fn embed_page(self: &Rc<Self>, pin_id: &str, pages: &gtk4::Stack) -> Box {
        let page = Box::new(Orientation::Vertical, 8);
        page.append(&back_title(
            pages,
            "получить код для встраивания",
            "more",
            &self.hold,
        ));
        let aspect = self.open_aspect.get();
        let pin = pin_id.to_string();
        let width = Rc::new(Cell::new(345i32));
        let code = Label::new(Some(&embed_code(&pin, 345, aspect)));
        code.set_wrap(true);
        code.set_max_width_chars(32);
        code.set_width_chars(32);
        code.set_selectable(true);
        code.set_xalign(0.0);
        code.add_css_class("pin-section");
        let sizes = Box::new(Orientation::Horizontal, 6);
        sizes.set_halign(Align::Center);
        let mut size_buttons = Vec::new();
        for (label, px) in [("малый", 237), ("средний", 345), ("большой", 450), ("сверхбольшой", 600)] {
            let button = Button::with_label(label);
            button.add_css_class("pin-size");
            if px == 345 {
                button.add_css_class("on");
            }
            size_buttons.push(button.clone());
            let width = width.clone();
            let code = code.clone();
            let pin = pin.clone();
            let group = Rc::new(RefCell::new(Vec::<Button>::new()));
            let group_click = group.clone();
            button.connect_clicked(move |button| {
                width.set(px);
                code.set_text(&embed_code(&pin, px, aspect));
                for other in group_click.borrow().iter() {
                    other.remove_css_class("on");
                }
                button.add_css_class("on");
            });
            let _ = group;
            sizes.append(&button);
        }
        let group: Rc<RefCell<Vec<Button>>> = Rc::new(RefCell::new(size_buttons.clone()));
        // Rebind so the clicked handler sees the full group. The closures above
        // captured empty groups, so wire the highlight here instead.
        for button in &size_buttons {
            let group = group.clone();
            button.connect_clicked(move |button| {
                for other in group.borrow().iter() {
                    other.remove_css_class("on");
                }
                button.add_css_class("on");
            });
        }
        page.append(&sizes);
        page.append(&code);
        let ok = Button::with_label("ok");
        ok.add_css_class("pin-board");
        let code_ok = code.clone();
        let menu = self.clone();
        let ok_click = ok.clone();
        ok.connect_clicked(move |_| {
            ok_click.clipboard().set_text(&code_ok.text());
            for sheet in &menu.sheets {
                sheet.popdown();
            }
            menu.flash("код скопирован");
        });
        page.append(&ok);
        page
    }

    fn fill_boards(self: &Rc<Self>, list: &Box, pin_id: &str, query: &str) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let gen = self.list_gen.get().wrapping_add(1);
        self.list_gen.set(gen);
        let catalog = self.catalog.borrow().clone();
        let q = query.trim().to_lowercase();
        let quick_id = catalog.quick.as_ref().map(|b| b.id.clone());

        let matches = |board: &BoardDest| {
            q.is_empty() || board.name.to_lowercase().contains(&q)
        };

        if q.is_empty() {
            if let Some(quick) = catalog.quick.clone().filter(|b| matches(b)) {
                list.append(&self.board_row(pin_id, &quick, gen));
                let tick = Box::new(Orientation::Horizontal, 0);
                tick.add_css_class("pin-tick");
                tick.set_size_request(36, 3);
                tick.set_halign(Align::Start);
                tick.set_margin_start(8);
                tick.set_margin_bottom(8);
                list.append(&tick);
            }
        }

        let mut any = false;
        if q.is_empty() && catalog.boards.iter().any(|b| quick_id.as_ref() != Some(&b.id)) {
            let section = Label::new(Some("все доски"));
            section.add_css_class("pin-section");
            section.set_halign(Align::Start);
            section.set_margin_start(8);
            section.set_margin_top(4);
            section.set_margin_bottom(4);
            list.append(&section);
        }
        for board in &catalog.boards {
            if !matches(board) {
                continue;
            }
            if q.is_empty() && quick_id.as_ref() == Some(&board.id) {
                continue;
            }
            list.append(&self.board_row(pin_id, board, gen));
            any = true;
        }
        if q.is_empty() && catalog.quick.is_some() {
            any = true;
        }
        if !any {
            let empty = if catalog.boards.is_empty() && catalog.quick.is_none() {
                "доски загружаются…"
            } else {
                "ничего не нашлось"
            };
            let label = Label::new(Some(empty));
            label.add_css_class("pin-section");
            label.set_halign(Align::Start);
            label.set_margin_start(8);
            label.set_margin_top(8);
            label.set_margin_bottom(8);
            list.append(&label);
        }
    }

    fn board_row(self: &Rc<Self>, pin_id: &str, board: &BoardDest, gen: u64) -> Button {
        let row = Button::new();
        row.add_css_class("pin-board");
        let inner = Box::new(Orientation::Horizontal, 12);
        inner.set_halign(Align::Fill);

        let frame = Box::new(Orientation::Horizontal, 0);
        frame.add_css_class("pin-cover");
        frame.set_size_request(44, 44);
        frame.set_overflow(Overflow::Hidden);
        let picture = Picture::new();
        picture.set_size_request(44, 44);
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk4::ContentFit::Cover);
        frame.append(&picture);
        if let Some(url) = board.cover.clone() {
            let cache = self.cache.clone();
            let runtime = self.runtime.clone();
            let picture = picture.clone();
            let menu = self.clone();
            glib::spawn_future_local(async move {
                let fetched = runtime
                    .spawn(async move { cache.fetch(&url).await })
                    .await;
                if menu.list_gen.get() != gen {
                    return;
                }
                if let Ok(Ok(path)) = fetched {
                    picture.set_filename(Some(&path));
                }
            });
        }
        inner.append(&frame);

        let name = Label::new(Some(&board.name));
        name.set_halign(Align::Start);
        name.set_hexpand(true);
        name.set_xalign(0.0);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        name.set_width_chars(16);
        name.set_max_width_chars(16);
        inner.append(&name);

        row.set_child(Some(&inner));
        let menu = self.clone();
        let pin_id = pin_id.to_string();
        let board = board.clone();
        row.connect_clicked(move |_| {
            menu.save_to(pin_id.clone(), Some(board.clone()));
        });
        row
    }

    fn create_row(self: &Rc<Self>, pin_id: &str) -> Box {
        let wrap = Box::new(Orientation::Horizontal, 12);
        wrap.set_margin_top(6);
        wrap.add_css_class("pin-create");

        let mark = DrawingArea::new();
        mark.add_css_class("pin-plus");
        mark.set_content_width(36);
        mark.set_content_height(36);
        mark.set_halign(Align::Center);
        mark.set_valign(Align::Center);
        mark.set_draw_func(|_, cr, w, h| draw_plus(cr, w, h));

        let name = Label::new(Some("создать доску"));
        name.add_css_class("pin-create-name");
        name.set_halign(Align::Start);
        name.set_hexpand(true);
        name.set_xalign(0.0);

        let entry = Entry::new();
        entry.add_css_class("pin-create-entry");
        entry.set_placeholder_text(Some("название"));
        self.watch_entry(&entry);
        entry.set_has_frame(false);
        entry.set_hexpand(true);
        entry.set_visible(false);

        let button = Button::new();
        button.add_css_class("pin-create");
        button.set_hexpand(true);
        let shown = Box::new(Orientation::Horizontal, 12);
        shown.append(&mark);
        shown.append(&name);
        shown.append(&entry);
        button.set_child(Some(&shown));

        let entry_show = entry.clone();
        let name_hide = name.clone();
        button.connect_clicked(move |_| {
            if WidgetExt::is_visible(&entry_show) {
                return;
            }
            name_hide.set_visible(false);
            entry_show.set_visible(true);
            entry_show.grab_focus();
        });

        let menu = self.clone();
        let pin_id = pin_id.to_string();
        let entry_done = entry.clone();
        let name_back = name.clone();
        entry.connect_activate(move |entry| {
            let title = entry.text().to_string();
            if title.trim().is_empty() {
                return;
            }
            let menu = menu.clone();
            let pin_id = pin_id.clone();
            let entry = entry_done.clone();
            let name = name_back.clone();
            glib::spawn_future_local(async move {
                menu.make_board(title, pin_id).await;
                entry.set_text("");
                entry.set_visible(false);
                name.set_visible(true);
            });
        });

        wrap.append(&button);
        wrap
    }

    async fn make_board(self: &Rc<Self>, name: String, pin_id: String) {
        if self.busy.get() {
            return;
        }
        self.busy.set(true);
        let runtime = self.runtime.clone();
        let result = runtime
            .spawn(async move { actions::create_board(&name).await })
            .await;
        self.busy.set(false);
        match result {
            Ok(Ok(board)) => {
                self.reload_catalog().await;
                if !board.id.is_empty() {
                    let mut catalog = self.catalog.borrow_mut();
                    if catalog.boards.iter().all(|b| b.id != board.id) {
                        catalog.boards.insert(0, board.clone());
                    }
                    if catalog.quick.is_none() {
                        catalog.quick = Some(board);
                    }
                }
                self.refill_open(&pin_id);
            }
            Ok(Err(err)) => self.flash(&err.to_string()),
            Err(err) => self.flash(&err.to_string()),
        }
    }

    fn refill_open(self: &Rc<Self>, pin_id: &str) {
        let monitor = self.last_monitor.get();
        let Some(sheet) = self.sheets.get(monitor) else {
            return;
        };
        if !sheet.is_visible() {
            return;
        }
        let query = sheet
            .child()
            .as_ref()
            .and_then(|child| find_search(child))
            .map(|entry| entry.text().to_string())
            .unwrap_or_default();
        sheet.set_child(Some(&self.build_sheet(pin_id)));
        if let Some(entry) = sheet
            .child()
            .as_ref()
            .and_then(|child| find_search(child))
        {
            entry.set_text(&query);
        }
    }

    fn save_to(self: &Rc<Self>, pin_id: String, board: Option<BoardDest>) {
        if self.busy.get() {
            return;
        }
        self.busy.set(true);
        for sheet in &self.sheets {
            sheet.popdown();
        }
        let menu = self.clone();
        let runtime = self.runtime.clone();
        glib::spawn_future_local(async move {
            let board_for_task = board.clone();
            let result = runtime
                .spawn(async move { actions::save_pin(&pin_id, board_for_task.as_ref()).await })
                .await;
            menu.busy.set(false);
            let text = match result {
                Ok(Ok(msg)) => msg,
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
            };
            menu.flash(&text);
        });
    }

    fn flash_undo(&self, text: &str, undo: Rc<dyn Fn()>) {
        self.present_note(text, 6);
        let monitor = self.last_monitor.get();
        if let Some(button) = self.undo_buttons.get(monitor) {
            button.set_visible(true);
        }
        *self.undo_cb.borrow_mut() = Some(undo);
    }

    fn flash(&self, text: &str) {
        self.present_note(text, 2);
    }

    fn present_note(&self, text: &str, secs: u64) {
        let monitor = self.last_monitor.get();
        let text = clip_note(text);
        if let Some(sheet) = self.sheets.get(monitor) {
            if sheet.is_visible() {
                if let Some(label) = self.status.borrow().as_ref() {
                    label.set_text(&text);
                    label.set_visible(true);
                }
                return;
            }
        }
        let Some(note) = self.notes.get(monitor) else {
            return;
        };
        let Some(label) = self.note_labels.get(monitor) else {
            return;
        };
        let Some(sheet) = self.sheets.get(monitor) else {
            return;
        };
        label.set_text(&text);
        if let Some(undo) = self.undo_buttons.get(monitor) {
            undo.set_visible(false);
        }
        *self.undo_cb.borrow_mut() = None;
        let (has_rect, rect) = sheet.pointing_to();
        if has_rect {
            note.set_pointing_to(Some(&rect));
        }
        note.popup();
        let note = note.clone();
        glib::timeout_add_local_once(Duration::from_secs(secs), move || {
            note.popdown();
        });
    }

    fn watch_entry(&self, entry: &Entry) {
        let typing = self.typing.clone();
        let held = Rc::new(Cell::new(false));
        entry.connect_has_focus_notify(move |entry| {
            let on = entry.has_focus();
            if on == held.get() {
                return;
            }
            held.set(on);
            if on {
                typing.set(typing.get().saturating_add(1));
            } else {
                typing.set(typing.get().saturating_sub(1));
            }
        });
    }

    pub fn dismiss(&self) {
        self.hold.open.set(false);
        self.hold.gen.set(self.hold.gen.get().wrapping_add(1));
        for sheet in &self.sheets {
            if sheet.is_visible() {
                sheet.popdown();
            }
        }
    }

    pub fn is_open(&self) -> bool {
        self.sheets.iter().any(|sheet| sheet.is_visible())
    }

    /// Closes the sheet when Hyprland focuses a different window.
    pub fn on_window_focus(&self, address: &str, when_ms: u128) {
        if address.is_empty() || self.hold.open.get() || !self.is_open() {
            return;
        }
        if when_ms < self.opened_ms.get() {
            return;
        }
        let baseline = norm_window(&self.focus_at_open.borrow());
        if norm_window(address) == baseline {
            return;
        }
        // The first event after launch may just be the window that was
        // already focused. Adopt it instead of closing the sheet.
        if baseline.is_empty() && when_ms < self.opened_ms.get().saturating_add(400) {
            self.focus_at_open.replace(address.to_string());
            return;
        }
        self.dismiss();
    }

    async fn reload_catalog(&self) {
        let runtime = self.runtime.clone();
        let loaded = runtime.spawn(actions::load_catalog()).await;
        match loaded {
            Ok(Ok(catalog)) => {
                *self.catalog.borrow_mut() = catalog;
            }
            Ok(Err(e)) => tracing::warn!("доски: {e:#}"),
            Err(e) => tracing::warn!("доски: {e}"),
        }
    }
}

fn set_typing(scroll: &ScrolledWindow, on: bool) {
    let Some(root) = scroll.root() else {
        return;
    };
    let Ok(win) = root.downcast::<gtk4::Window>() else {
        return;
    };
    win.set_keyboard_mode(if on {
        KeyboardMode::OnDemand
    } else {
        KeyboardMode::None
    });
}

fn show_page(pages: &gtk4::Stack, name: &str, hold: &Rc<SheetHold>) {
    arm_hold(hold);
    let pages = pages.clone();
    let name = name.to_string();
    glib::idle_add_local_once(move || {
        if pages.parent().is_some() {
            pages.set_visible_child_name(&name);
        }
    });
}

fn arm_hold(hold: &Rc<SheetHold>) {
    hold.open.set(true);
    let gen = hold.gen.get().wrapping_add(1);
    hold.gen.set(gen);
    let hold = hold.clone();
    glib::timeout_add_local_once(Duration::from_millis(350), move || {
        if hold.gen.get() == gen {
            hold.open.set(false);
        }
    });
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn focus_stamp_path() -> Option<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(std::path::PathBuf::from(runtime).join("pinterest-wallpaper-focus"))
}

fn norm_window(address: &str) -> String {
    address
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

fn current_window_address() -> String {
    let Some(path) = focus_stamp_path() else {
        return String::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    text.split('\t').next().unwrap_or("").trim().to_string()
}

fn rail_button(glyph: &str, tip: &str) -> Button {
    let button = Button::new();
    button.add_css_class("pin-rail-btn");
    button.set_tooltip_text(Some(tip));
    let label = Label::new(Some(glyph));
    label.add_css_class("pin-rail-glyph");
    button.set_child(Some(&label));
    button
}

fn clip_note(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut out: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        out.push('…');
    }
    out
}

fn draw_plus(cr: &gtk4::cairo::Context, width: i32, height: i32) {
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.select_font_face(
        "Sans",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Bold,
    );
    cr.set_font_size(22.0);
    let Ok(ext) = cr.text_extents("+") else {
        return;
    };
    let x = (f64::from(width) - ext.width()) / 2.0 - ext.x_bearing();
    let y = (f64::from(height) - ext.height()) / 2.0 - ext.y_bearing();
    cr.move_to(x, y);
    let _ = cr.show_text("+");
}

fn comments_title(count: usize) -> String {
    let n10 = count % 10;
    let n100 = count % 100;
    let word = if (11..=14).contains(&n100) {
        "комментариев"
    } else if n10 == 1 {
        "комментарий"
    } else if (2..=4).contains(&n10) {
        "комментария"
    } else {
        "комментариев"
    };
    if count == 0 {
        "комментариев пока нет".to_string()
    } else {
        format!("{count} {word}")
    }
}

fn clip_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn fit_sheet_width(frame: &ScrolledWindow, pages: &gtk4::Stack, hold: &Rc<SheetHold>) {
    let Some(child) = pages.visible_child() else {
        return;
    };
    let (_, natural, _, _) = child.measure(Orientation::Horizontal, -1);
    let width = natural.clamp(240, 400);
    if frame.width_request() != width {
        arm_hold(hold);
        frame.set_width_request(width);
    }
}

fn choice_row(
    title: &str,
    hint: &str,
    group: &Rc<RefCell<Vec<(Button, Box)>>>,
    chosen: &Rc<RefCell<Option<String>>>,
    key: &str,
) -> Box {
    let wrap = Box::new(Orientation::Vertical, 0);
    let row = Button::new();
    row.add_css_class("pin-choice");
    let inner = Box::new(Orientation::Horizontal, 8);
    let dot = Box::new(Orientation::Horizontal, 0);
    dot.add_css_class("pin-dot");
    dot.set_size_request(14, 14);
    dot.set_halign(Align::Center);
    dot.set_valign(Align::Center);
    let label = Label::new(Some(title));
    label.set_wrap(true);
    label.set_width_chars(28);
    label.set_max_width_chars(28);
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    label.set_xalign(0.0);
    inner.append(&dot);
    inner.append(&label);
    row.set_child(Some(&inner));

    let hint_label = section_label(hint);
    hint_label.set_margin_start(22);
    hint_label.set_margin_top(0);
    hint_label.set_margin_bottom(4);
    hint_label.set_width_chars(30);
    hint_label.set_max_width_chars(30);

    let group_click = group.clone();
    let chosen = chosen.clone();
    let key = key.to_string();
    let dot_mark = dot.clone();
    row.connect_clicked(move |button| {
        for (other, other_dot) in group_click.borrow().iter() {
            other.remove_css_class("on");
            other_dot.remove_css_class("on");
        }
        button.add_css_class("on");
        dot_mark.add_css_class("on");
        *chosen.borrow_mut() = Some(key.clone());
    });
    group.borrow_mut().push((row.clone(), dot));
    wrap.append(&row);
    wrap.append(&hint_label);
    wrap
}

fn section_label(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class("pin-section");
    label.set_halign(Align::Start);
    label.set_wrap(true);
    label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    label.set_max_width_chars(36);
    label.set_xalign(0.0);
    label.set_margin_start(8);
    label.set_margin_top(6);
    label.set_margin_bottom(6);
    label
}

fn action_button_cb(title: &str, on_click: impl Fn(&Button) + 'static) -> Button {
    let button = action_button(title);
    button.connect_clicked(on_click);
    button
}

fn stack_button(pages: &gtk4::Stack, title: &str, name: &str, hold: &Rc<SheetHold>) -> Button {
    let pages = pages.clone();
    let name = name.to_string();
    let hold = hold.clone();
    action_button_cb(title, move |_| {
        show_page(&pages, &name, &hold);
    })
}

fn back_title(pages: &gtk4::Stack, text: &str, back: &str, hold: &Rc<SheetHold>) -> Box {
    let row = Box::new(Orientation::Horizontal, 8);
    let back_btn = Button::with_label("←");
    back_btn.add_css_class("pin-rail-btn");
    let pages = pages.clone();
    let back = back.to_string();
    let hold = hold.clone();
    back_btn.connect_clicked(move |_| {
        show_page(&pages, &back, &hold);
    });
    let title = Label::new(Some(text));
    title.add_css_class("pin-title");
    title.set_hexpand(true);
    title.set_wrap(true);
    title.set_halign(Align::Center);
    row.append(&back_btn);
    row.append(&title);
    row
}

fn fill_comment_list(
    list: &Box,
    comments: &[actions::PinComment],
    reply_to: &Rc<RefCell<Option<String>>>,
    entry: &Entry,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    if comments.is_empty() {
        list.append(&section_label("комментариев пока нет"));
        return;
    }
    for comment in comments {
        let row = Box::new(Orientation::Vertical, 2);
        let head = Label::new(Some(&format!("{}  {}", comment.author, comment.text)));
        head.set_halign(Align::Start);
        head.set_wrap(true);
        head.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        head.set_max_width_chars(36);
        head.set_xalign(0.0);
        row.append(&head);
        let meta = Box::new(Orientation::Horizontal, 10);
        if !comment.when.is_empty() {
            let when = section_label(&comment.when);
            when.set_margin_top(0);
            when.set_margin_bottom(0);
            meta.append(&when);
        }
        let reply = Button::with_label("ответить");
        reply.add_css_class("pin-board");
        let reply_to = reply_to.clone();
        let entry = entry.clone();
        let id = comment.id.clone();
        let author = comment.author.clone();
        reply.connect_clicked(move |_| {
            *reply_to.borrow_mut() = if id.is_empty() { None } else { Some(id.clone()) };
            entry.set_placeholder_text(Some(&format!("ответ для {author}")));
            entry.grab_focus();
        });
        meta.append(&reply);
        if comment.likes > 0 {
            meta.append(&section_label(&format!("♥ {}", comment.likes)));
        }
        row.append(&meta);
        list.append(&row);
    }
}

fn share_person_row(menu: &Rc<PinMenu>, pin_id: &str, person: &actions::SharePerson) -> Box {
    let row = Box::new(Orientation::Horizontal, 8);
    let text = Box::new(Orientation::Vertical, 0);
    text.set_hexpand(true);
    let name = Label::new(Some(&clip_chars(&person.name, 22)));
    name.set_halign(Align::Start);
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    name.set_width_chars(16);
    name.set_max_width_chars(16);
    let handle = section_label(&format!("@{}", clip_chars(person.handle.trim_start_matches('@'), 16)));
    handle.set_margin_top(0);
    handle.set_margin_bottom(0);
    handle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    handle.set_width_chars(16);
    handle.set_max_width_chars(16);
    text.append(&name);
    text.append(&handle);
    let send = Button::with_label("отправить");
    send.add_css_class("pin-board");
    let menu = menu.clone();
    let pin_id = pin_id.to_string();
    let user_id = person.id.clone();
    send.connect_clicked(move |button| {
        button.set_sensitive(false);
        let menu = menu.clone();
        let pin_id = pin_id.clone();
        let user_id = user_id.clone();
        let button = button.clone();
        glib::spawn_future_local(async move {
            let runtime = menu.runtime.clone();
            let result = runtime
                .spawn(async move { actions::send_pin(&pin_id, &user_id).await })
                .await;
            match result {
                Ok(Ok(())) => {
                    if let Some(label) = button.child().and_downcast::<Label>() {
                        label.set_text("отправлено");
                    }
                }
                Ok(Err(err)) => {
                    button.set_sensitive(true);
                    menu.flash(&err.to_string());
                }
                Err(_) => {
                    button.set_sensitive(true);
                    menu.flash("не удалось отправить");
                }
            }
        });
    });
    row.append(&text);
    row.append(&send);
    row
}

fn collage_row(menu: &Rc<PinMenu>, pin_id: &str, collage: &actions::CollageDest) -> Button {
    let menu = menu.clone();
    let pin_id = pin_id.to_string();
    let collage_id = collage.id.clone();
    let name = collage.name.clone();
    action_button_cb(&name.clone(), move |_| {
        let menu = menu.clone();
        let pin_id = pin_id.clone();
        let collage_id = collage_id.clone();
        let name = name.clone();
        glib::spawn_future_local(async move {
            let runtime = menu.runtime.clone();
            let result = runtime
                .spawn(async move { actions::add_to_collage(&collage_id, &pin_id).await })
                .await;
            match result {
                Ok(Ok(())) => menu.flash(&format!("добавлено в «{name}»")),
                Ok(Err(err)) => menu.flash(&err.to_string()),
                Err(_) => menu.flash("не удалось добавить в коллаж"),
            }
        });
    })
}

fn feedback_button(
    menu: &Rc<PinMenu>,
    pin_id: &str,
    kind: &str,
    reason: &str,
    title: &str,
) -> Button {
    let menu = menu.clone();
    let pin_id = pin_id.to_string();
    let kind = kind.to_string();
    let reason = reason.to_string();
    action_button_cb(title, move |_| {
        let menu = menu.clone();
        let pin_id = pin_id.clone();
        let kind = kind.clone();
        let reason = reason.clone();
        glib::spawn_future_local(async move {
            let runtime = menu.runtime.clone();
            let result = runtime
                .spawn(async move {
                    actions::pin_feedback(&pin_id, &kind, Some(&reason), false).await
                })
                .await;
            match result {
                Ok(Ok(())) => {
                    for sheet in &menu.sheets {
                        sheet.popdown();
                    }
                    menu.flash("будем показывать меньше таких пинов");
                }
                Ok(Err(err)) => menu.flash(&err.to_string()),
                Err(_) => menu.flash("не удалось"),
            }
        });
    })
}

fn report_reasons() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("спам", "вводящие в заблуждение или повторяющиеся публикации", "spam"),
        (
            "изображения обнаженного тела, порнография или содержимое сексуального характера",
            "содержимое сексуального характера",
            "nudity",
        ),
        (
            "членовредительство",
            "расстройства пищевого поведения, нанесение травм себе, суицид",
            "self-harm",
        ),
        (
            "ложная информация",
            "ложная информация о здоровье, климате, голосованиях или теории заговора",
            "misinformation",
        ),
        (
            "агрессивные действия",
            "предрассудки, стереотипы, идея превосходства, оскорбления",
            "hate-speech",
        ),
        ("опасные товары", "наркотики, оружие, регулируемые товары", "dangerous-goods"),
        (
            "преследование или критика",
            "оскорбления, угрозы, кибербуллинг, опубликованные без разрешения",
            "harassment",
        ),
        (
            "сцены насилия или изображение лиц, причастных к насильственным действиям",
            "графическое изображение или пропаганда насилия",
            "violence",
        ),
        (
            "нарушение конфиденциальности",
            "личные фотографии, персональная информация",
            "privacy",
        ),
        (
            "интимные изображения, произведенные без согласия",
            "интимные изображения вас или человека, которого вы представляете",
            "non-consensual",
        ),
        ("это моя интеллектуальная собственность", "", "intellectual-property"),
    ]
}

fn embed_code(pin_id: &str, width: i32, aspect: f64) -> String {
    let height = (f64::from(width) * aspect).round().max(120.0) as i32;
    format!(
        "<iframe src=\"https://assets.pinterest.com/ext/embed.html?id={pin_id}\" height=\"{height}\" width=\"{width}\" frameborder=\"0\" scrolling=\"no\"></iframe>"
    )
}

fn copy_download(path: &std::path::Path, pin_id: &str) -> Result<String, String> {
    let dir = dirs::download_dir().ok_or_else(|| "нет папки Загрузки".to_string())?;
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("jpg");
    let dest = dir.join(format!("pinterest-{pin_id}.{ext}"));
    std::fs::copy(path, &dest).map_err(|err| err.to_string())?;
    Ok(dest.display().to_string())
}

fn action_button(title: &str) -> Button {
    let button = Button::new();
    button.add_css_class("pin-board");
    button.set_hexpand(true);
    let label = Label::new(Some(title));
    label.set_halign(Align::Start);
    label.set_xalign(0.0);
    button.set_child(Some(&label));
    button
}

fn find_search(widget: &Widget) -> Option<Entry> {
    if let Some(entry) = widget.downcast_ref::<Entry>() {
        if entry.has_css_class("pin-search") {
            return Some(entry.clone());
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_search(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}
