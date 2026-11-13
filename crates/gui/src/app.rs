//! The settings window. A GTK3 clone of OpenTabletDriver's UX: OTD draws
//! its window with GTK3 on Linux, so using the same toolkit makes kerntablet
//! pick up the user's GTK theme (gtk-3.0/settings.ini, gtk.css) exactly the
//! way OTD does. Nothing here is themed by hand; the area drawings take their
//! colours from the theme's named colours.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{cairo, gdk, glib, pango};
use kerntablet_common::bpfmap::{self, StateReader};
use kerntablet_common::config::{self, BindMode, Config, DisplayArea, TabletArea};
use kerntablet_common::desktop::{self, Desktop};
use kerntablet_common::kernel::KtState;
use kerntablet_common::plugin::{self, Index, ParamKind, Plugin, PluginEntry, PluginSettings, Value};
use kerntablet_common::tablet::{TabletId, TabletSpec, TABLETS};
use kerntablet_common::transform;

use crate::core::{self, Health, Probe};

pub fn run() -> Result<(), String> {
    glib::set_prgname(Some("kerntablet"));
    glib::set_application_name("kerntablet");
    gtk::init().map_err(|e| format!("cannot open a window: {e}"))?;
    install_rounding();
    let app = App::new();
    app.ui.window.show_all();
    gtk::main();
    Ok(())
}

// ---------------------------------------------------------------- state

/// Result of a background job (network / password prompt).
enum JobResult {
    Index(Result<Index, String>),
    Done(Result<String, String>),
}

struct Model {
    probe: Probe,
    desktop: Desktop,
    desktop_error: Option<String>,
    /// Settings in the editor. Edits live only here until Apply.
    cfg: Config,
    /// What was last applied (= saved); Reset goes back to it.
    baseline: Config,
    pbaseline: PluginSettings,
    plugins: Vec<Plugin>,
    plugin_errors: Vec<String>,
    psettings: PluginSettings,
    index: Option<Result<Index, String>>,
    job: Option<(String, Receiver<JobResult>)>,
    reader: Option<(String, StateReader)>,
    pen: Option<KtState>,
    last_probe: Instant,
    last_desktop: Instant,
    /// Plugin shown on the right of the Filters tab / in the plugin manager.
    filter_sel: Option<String>,
    manager_sel: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Canvas {
    Display,
    Tablet,
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    DW,
    DH,
    DX,
    DY,
    TW,
    TH,
    TX,
    TY,
    TR,
}

struct Ui {
    window: gtk::Window,
    output: gtk::Fixed,
    tablet_name: gtk::Label,
    display_row: gtk::Box,
    tablet_row: gtk::Box,
    display: gtk::DrawingArea,
    tablet: gtk::DrawingArea,
    entries: Vec<(Field, gtk::Entry)>,
    tablets_menu: gtk::Menu,
    discard: gtk::Button,
    status: gtk::Label,
    message: gtk::Label,
    console: gtk::TextBuffer,
    filter_list: gtk::ListBox,
    filter_detail: gtk::Box,
}

struct Manager {
    window: gtk::Window,
    list: gtk::ListBox,
    detail: gtk::Box,
    spinner: gtk::Spinner,
    status: gtk::Label,
}

struct App {
    ui: Ui,
    m: RefCell<Model>,
    /// Set while widgets are being filled from the model, so their change
    /// signals do not echo back into it.
    updating: Cell<bool>,
    /// Canvas being dragged and the last pointer position.
    drag: Cell<Option<(Canvas, f64, f64)>>,
    /// Keeps the open context menu alive.
    popup: RefCell<Option<gtk::Menu>>,
    manager: RefCell<Option<Manager>>,
    /// Connected tablets listed in the Tablets menu, and their menu items.
    tablet_items: RefCell<(Vec<String>, Vec<gtk::CheckMenuItem>)>,
}

// ---------------------------------------------------------------- layout

// Spacing scale, in px. Every gap, margin and padding in the window is one
// of these; nothing else.
/// A label and its own control; a unit after a field.
const SP_TIGHT: i32 = 4;
/// Related items next to each other; the outer padding of every pane.
const SP: i32 = 8;
/// Between sections.
const SP_SECTION: i32 = 16;
/// Width of every numeric field, in characters.
const FIELD_CHARS: i32 = 7;
/// Width of the list on the left of the Filters tab and the plugin manager.
const SIDE_LIST: i32 = 160;

impl App {
    fn new() -> Rc<Self> {
        let (desktop, desktop_error) = match desktop::detect() {
            Ok(d) => (d, None),
            Err(e) => (Desktop::fallback(), Some(e)),
        };
        let probe = core::probe(core::saved_preference());
        let had_saved = matches!(Config::load(), Ok(Some(_)));
        let (cfg, origin) = core::initial_config(probe.spec, &desktop);
        let (plugins, plugin_errors) = plugin::installed();
        let psettings = core::plugin_settings(&plugins);
        let now = Instant::now();
        let model = Model {
            probe,
            desktop,
            desktop_error,
            cfg,
            baseline: cfg,
            pbaseline: psettings.clone(),
            plugins,
            plugin_errors,
            psettings,
            index: None,
            job: None,
            reader: None,
            pen: None,
            last_probe: now,
            last_desktop: now,
            filter_sel: None,
            manager_sel: None,
        };

        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("kerntablet");
        window.set_icon_name(Some("input-tablet"));
        window.set_default_size(720, 680);
        let accel = gtk::AccelGroup::new();
        window.add_accel_group(&accel);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // Header: app icon and name, then the menu, on one line.
        let header = gtk::Box::new(gtk::Orientation::Horizontal, SP);
        header.set_margin_start(SP_SECTION);
        header.set_margin_end(SP_SECTION);
        header.set_margin_top(SP_TIGHT);
        header.set_margin_bottom(SP_TIGHT);
        let app_icon = gtk::Image::from_icon_name(Some("input-tablet-symbolic"), gtk::IconSize::LargeToolbar);
        let app_name = gtk::Label::new(None);
        app_name.set_markup("<b>kerntablet</b>");
        app_name.set_margin_end(SP_SECTION);
        let menubar = gtk::MenuBar::new();
        menubar.style_context().add_class("kt-chrome");
        header.pack_start(&app_icon, false, false, 0);
        header.pack_start(&app_name, false, false, 0);
        header.pack_start(&menubar, false, false, 0);
        root.pack_start(&header, false, false, 0);
        root.pack_start(&gtk::Separator::new(gtk::Orientation::Horizontal), false, false, 0);

        // Tabs: a notebook of empty pages (themed exactly like tabs) switching
        // a stack that holds the real pages, so the tab strip can have the
        // same side margins as the content.
        let tabs = gtk::Notebook::new();
        tabs.set_show_border(false);
        tabs.set_margin_start(SP_SECTION);
        tabs.set_margin_end(SP_SECTION);
        tabs.set_margin_top(SP);
        let pages = gtk::Stack::new();
        pages.set_transition_type(gtk::StackTransitionType::None);
        root.pack_start(&tabs, false, false, 0);
        root.pack_start(&pages, true, true, 0);
        root.pack_start(&gtk::Separator::new(gtk::Orientation::Horizontal), false, false, 0);
        let add_page = |child: &gtk::Widget, name: &str| {
            pages.add_named(child, name);
            let stub = gtk::Box::new(gtk::Orientation::Vertical, 0);
            tabs.append_page(&stub, Some(&gtk::Label::new(Some(name))));
        };
        tabs.connect_switch_page(glib::clone!(@weak pages => move |_, _, n| {
            if let Some(child) = pages.children().get(n as usize) {
                pages.set_visible_child(child);
            }
        }));

        // Output tab: two previews, each with its row of fields right under
        // it. Positions come from `output_layout`, recomputed on every
        // allocation (see `App::relayout`), never from a cached size.
        let output = gtk::Fixed::new();
        let display = canvas();
        let tablet = canvas();
        let mut entries = Vec::new();
        // Top to bottom: monitors, "Display ▾" with their fields; the tablet,
        // "Tablet ▾" with the area's fields and rotation.
        let display_row = field_row(
            &mut entries,
            "Display",
            &[(Field::DW, "W", "Width, px"), (Field::DH, "H", "Height, px"), (Field::DX, "X", "Centre X, px"), (Field::DY, "Y", "Centre Y, px")],
            "px",
            false,
        );
        let tablet_row = field_row(
            &mut entries,
            "Tablet",
            &[
                (Field::TW, "W", "Width, mm"),
                (Field::TH, "H", "Height, mm"),
                (Field::TX, "X", "Centre X, mm"),
                (Field::TY, "Y", "Centre Y, mm"),
                (Field::TR, "", "Rotation, degrees"),
            ],
            "mm",
            true,
        );
        for w in [display.upcast_ref::<gtk::Widget>(), display_row.row.upcast_ref(), tablet.upcast_ref(), tablet_row.row.upcast_ref()] {
            output.put(w, 0, 0);
        }
        add_page(output.upcast_ref(), "Output");

        // Filters tab.
        let filters = gtk::Paned::new(gtk::Orientation::Horizontal);
        let filter_list = gtk::ListBox::new();
        filter_list.set_selection_mode(gtk::SelectionMode::Browse);
        let left = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        left.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        left.set_size_request(SIDE_LIST, -1);
        left.add(&filter_list);
        let filter_detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
        filter_detail.set_border_width(SP as u32);
        let right = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        right.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        right.add(&filter_detail);
        filters.pack1(&left, false, false);
        filters.pack2(&right, true, false);
        add_page(filters.upcast_ref(), "Filters");

        // Console tab.
        let console_view = gtk::TextView::new();
        console_view.set_editable(false);
        console_view.set_cursor_visible(false);
        console_view.set_monospace(true);
        console_view.set_left_margin(SP);
        console_view.set_right_margin(SP);
        console_view.set_top_margin(SP);
        console_view.set_bottom_margin(SP);
        console_view.set_wrap_mode(gtk::WrapMode::WordChar);
        let console_scroll = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
        console_scroll.add(&console_view);
        add_page(console_scroll.upcast_ref(), "Console");
        let console = console_view.buffer().expect("text view has a buffer");
        // Keep the newest line in view.
        let end_mark = console.create_mark(Some("end"), &console.end_iter(), false).expect("text mark");
        console.connect_changed(glib::clone!(@weak console_view => move |b| {
            b.move_mark(&end_mark, &b.end_iter());
            console_view.scroll_mark_onscreen(&end_mark);
        }));

        // Footer: [tablet icon] Wacom CTL-472   ● Active      Reset  [✓ Apply]
        let footer = status_bar();
        let tablet_icon = gtk::Image::from_icon_name(Some("input-tablet-symbolic"), gtk::IconSize::Button);
        let tablet_name = gtk::Label::new(None);
        tablet_name.set_margin_end(SP_SECTION);
        let status = gtk::Label::new(None);
        let message = gtk::Label::new(None);
        message.set_ellipsize(pango::EllipsizeMode::End);
        message.set_xalign(0.0);
        message.set_margin_start(SP);
        message.style_context().add_class("dim-label");
        let discard = gtk::Button::with_label("Reset");
        discard.set_tooltip_text(Some("Go back to the applied settings"));
        discard.style_context().add_class("flat");
        discard.style_context().add_class("kt-action");
        let apply = gtk::Button::with_label("Apply");
        apply.style_context().add_class("kt-primary");
        apply.style_context().add_class("kt-action");
        apply.set_tooltip_text(Some(
            "Send the settings to the kernel and keep them: they survive replugging the tablet and logging in again (Ctrl+S). \
             Changes that are not applied are forgotten when the window closes.",
        ));
        footer.pack_start(&tablet_icon, false, false, 0);
        footer.pack_start(&tablet_name, false, false, 0);
        footer.pack_start(&status, false, false, 0);
        footer.pack_start(&message, true, true, 0);
        footer.pack_end(&apply, false, false, 0);
        footer.pack_end(&discard, false, false, 0);
        root.pack_start(&footer, false, false, 0);
        window.add(&root);

        let app = Rc::new(App {
            ui: Ui {
                window,
                output: output.clone(),
                display_row: display_row.row.clone(),
                tablet_row: tablet_row.row.clone(),
                tablet_name,
                display,
                tablet,
                entries,
                tablets_menu: gtk::Menu::new(),
                discard,
                status,
                message,
                console,
                filter_list,
                filter_detail,
            },
            m: RefCell::new(model),
            updating: Cell::new(false),
            drag: Cell::new(None),
            popup: RefCell::new(None),
            manager: RefCell::new(None),
            tablet_items: RefCell::new((Vec::new(), Vec::new())),
        });

        app.build_menu(&menubar, &accel);
        app.connect_rows(&display_row, &tablet_row);
        // The minimum depends on the theme's fonts and entry padding, which
        // are only final once the widgets are styled: measure on realize and
        // again whenever the theme changes.
        app.ui.output.connect_realize(glib::clone!(@weak app => move |_| app.update_output_min()));
        app.ui.window.connect_style_updated(glib::clone!(@weak app => move |_| app.update_output_min()));
        app.ui.output.connect_size_allocate(glib::clone!(@weak app => move |_, a| app.relayout(a)));
        app.connect_canvases();
        app.connect_entries();
        apply.connect_clicked(glib::clone!(@weak app => move |_| app.apply()));
        app.ui.discard.connect_clicked(glib::clone!(@weak app => move |_| app.discard()));
        app.ui.filter_list.connect_row_selected(glib::clone!(@weak app => move |_, row| {
            if app.updating.get() {
                return;
            }
            app.m.borrow_mut().filter_sel = row.map(|r| r.widget_name().to_string());
            app.rebuild_filter_detail();
        }));
        app.ui.window.connect_delete_event(|_, _| {
            gtk::main_quit();
            glib::Propagation::Proceed
        });

        {
            let m = app.m.borrow();
            app.log(&format!("{} — {}", m.desktop.backend.label(), origin));
            if let Some(e) = &m.desktop_error {
                app.log(&format!("Monitors: {e}"));
            }
            for e in &m.plugin_errors {
                app.log(&format!("Broken plugin {e}"));
            }
        }
        if !had_saved {
            app.note(&format!("{origin} — press Apply"), false);
        }
        app.rebuild_filters();
        app.refresh_status();
        app.sync();

        let tick = glib::clone!(@weak app => @default-return glib::ControlFlow::Break, move || {
            app.tick();
            glib::ControlFlow::Continue
        });
        glib::timeout_add_local(Duration::from_millis(16), tick);
        app
    }

    fn build_menu(self: &Rc<Self>, bar: &gtk::MenuBar, accel: &gtk::AccelGroup) {
        let file = submenu(bar, "_File");
        let apply = item(&file, "Apply", glib::clone!(@weak self as app => move || app.apply()));
        apply.add_accelerator("activate", accel, *gdk::keys::constants::s, gdk::ModifierType::CONTROL_MASK, gtk::AccelFlags::VISIBLE);
        item(&file, "Reset changes", glib::clone!(@weak self as app => move || app.discard()));
        file.append(&gtk::SeparatorMenuItem::new());
        item(&file, "Import from OpenTabletDriver", glib::clone!(@weak self as app => move || app.import_otd()))
            .set_tooltip_text(Some("Take the areas and matching filter settings from ~/.config/OpenTabletDriver/settings.json"));
        file.append(&gtk::SeparatorMenuItem::new());
        let quit = item(&file, "Quit", gtk::main_quit);
        quit.add_accelerator("activate", accel, *gdk::keys::constants::q, gdk::ModifierType::CONTROL_MASK, gtk::AccelFlags::VISIBLE);

        let top = gtk::MenuItem::with_mnemonic("_Tablets");
        top.set_submenu(Some(&self.ui.tablets_menu));
        bar.append(&top);
        let tablets = self.ui.tablets_menu.clone();
        item(&tablets, "Detect tablet", glib::clone!(@weak self as app => move || {
            app.redetect();
        }));
        let enabled = gtk::CheckMenuItem::with_label("Area mapping enabled");
        enabled.set_tooltip_text(Some("Off — the tablet behaves as if unconfigured: full surface on the whole desktop"));
        enabled.set_active(self.m.borrow().cfg.enabled);
        enabled.connect_toggled(glib::clone!(@weak self as app => move |c| {
            if !app.updating.get() {
                app.m.borrow_mut().cfg.enabled = c.is_active();
                app.update_dirty();
            }
        }));
        tablets.append(&enabled);
        tablets.append(&gtk::SeparatorMenuItem::new());
        // Keep the check box in step with Discard / import.
        let watch = enabled.clone();
        tablets.connect_show(glib::clone!(@weak self as app => move |_| {
            app.updating.set(true);
            watch.set_active(app.m.borrow().cfg.enabled);
            app.updating.set(false);
        }));

        let plugins = submenu(bar, "_Plugins");
        item(&plugins, "Plugin manager", glib::clone!(@weak self as app => move || app.open_manager()));
        item(&plugins, "Open plugin directory", glib::clone!(@weak self as app => move || {
            let dir = plugin::plugin_dir();
            if !dir.is_dir() {
                app.note(&format!("{} does not exist yet — no plugin has been installed", dir.display()), true);
                return;
            }
            // Opens with the desktop's default app for folders
            // (inode/directory in mimeapps.list).
            let uri = format!("file://{}", dir.display());
            if let Err(e) = gtk::show_uri_on_window(Some(&app.ui.window), &uri, gtk::current_event_time()) {
                app.note(&format!("Could not open {}: {e}", dir.display()), true);
            }
        }));

        let help = submenu(bar, "_Help");
        item(&help, "Status", glib::clone!(@weak self as app => move || app.log_status()));
        item(&help, "About", glib::clone!(@weak self as app => move || {
            let d = gtk::AboutDialog::new();
            d.set_transient_for(Some(&app.ui.window));
            d.set_program_name("kerntablet");
            d.set_version(Some(env!("CARGO_PKG_VERSION")));
            d.set_comments(Some("Tablet area mapping that runs in the kernel"));
            d.set_license_type(gtk::License::Gpl20Only);
            d.set_logo_icon_name(Some("input-tablet"));
            d.set_website(Some("https://github.com/freed0my/kerntablet"));
            d.run();
            unsafe { d.destroy() };
        }));
    }

    fn connect_canvases(self: &Rc<Self>) {
        for (da, which) in [(&self.ui.display, Canvas::Display), (&self.ui.tablet, Canvas::Tablet)] {
            da.connect_draw(glib::clone!(@weak self as app => @default-return glib::Propagation::Stop, move |da, cr| {
                match which {
                    Canvas::Display => app.draw_display(da, cr),
                    Canvas::Tablet => app.draw_tablet(da, cr),
                }
                glib::Propagation::Stop
            }));
            da.connect_button_press_event(glib::clone!(@weak self as app => @default-return glib::Propagation::Stop, move |da, ev| {
                if ev.event_type() != gdk::EventType::ButtonPress {
                    return glib::Propagation::Stop;
                }
                let (x, y) = ev.position();
                match ev.button() {
                    1 => app.drag.set(Some((which, x, y))),
                    3 => app.context_menu(da, which, ev),
                    _ => {}
                }
                glib::Propagation::Stop
            }));
            da.connect_motion_notify_event(glib::clone!(@weak self as app => @default-return glib::Propagation::Stop, move |da, ev| {
                if let Some((w, lx, ly)) = app.drag.get().filter(|d| d.0 == which) {
                    let (x, y) = ev.position();
                    app.drag.set(Some((w, x, y)));
                    app.drag_by(da, which, x - lx, y - ly);
                }
                glib::Propagation::Stop
            }));
            da.connect_button_release_event(glib::clone!(@weak self as app => @default-return glib::Propagation::Stop, move |_, ev| {
                if ev.button() == 1 && app.drag.take().is_some() {
                    {
                        let mut m = app.m.borrow_mut();
                        match which {
                            // Keep the screen area on whole pixels.
                            Canvas::Display => {
                                let d = &mut m.cfg.display;
                                d.x = (d.x - d.width / 2.0).round() + d.width / 2.0;
                                d.y = (d.y - d.height / 2.0).round() + d.height / 2.0;
                            }
                            Canvas::Tablet => {
                                let t = &mut m.cfg.tablet;
                                t.x = (t.x * 1000.0).round() / 1000.0;
                                t.y = (t.y * 1000.0).round() / 1000.0;
                            }
                        }
                    }
                    app.sync();
                }
                glib::Propagation::Stop
            }));
        }
    }

    fn connect_entries(self: &Rc<Self>) {
        for (field, e) in &self.ui.entries {
            let field = *field;
            e.connect_activate(glib::clone!(@weak self as app => move |e| app.commit(field, &e.text())));
            e.connect_focus_out_event(glib::clone!(@weak self as app => @default-return glib::Propagation::Proceed, move |e, _| {
                app.commit(field, &e.text());
                glib::Propagation::Proceed
            }));
        }
    }
}

fn canvas() -> gtk::DrawingArea {
    let da = gtk::DrawingArea::new();
    da.set_size_request(1, 1);
    da.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK | gdk::EventMask::BUTTON_RELEASE_MASK | gdk::EventMask::BUTTON1_MOTION_MASK,
    );
    da
}

/// The bottom bar of a window: [left items … message] [buttons].
fn status_bar() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, SP);
    b.set_margin_start(SP);
    b.set_margin_end(SP);
    b.set_margin_top(SP_TIGHT);
    b.set_margin_bottom(SP_TIGHT);
    b
}

/// One row of the Output tab: "Title ▾  W [..]  H [..]  X [..]  Y [..] unit"
/// and, for the tablet, "⟲ [180° ▾]".
struct FieldRow {
    row: gtk::Box,
    /// Opens the same menu as a right-click on the preview above.
    title: gtk::Button,
    rotation: Option<(gtk::Button, gtk::ComboBoxText)>,
}

fn field_row(out: &mut Vec<(Field, gtk::Entry)>, title: &str, fields: &[(Field, &str, &str)], unit: &str, rotation: bool) -> FieldRow {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let title_button = gtk::Button::new();
    title_button.style_context().add_class("flat");
    let tb = gtk::Box::new(gtk::Orientation::Horizontal, SP_TIGHT);
    let tl = gtk::Label::new(None);
    tl.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
    tb.pack_start(&tl, false, false, 0);
    tb.pack_start(&gtk::Image::from_icon_name(Some("pan-down-symbolic"), gtk::IconSize::Button), false, false, 0);
    title_button.add(&tb);
    title_button.set_margin_end(SP);
    row.pack_start(&title_button, false, false, 0);
    for (f, label, tip) in fields.iter().filter(|(f, ..)| *f != Field::TR) {
        let l = gtk::Label::new(Some(label));
        l.set_margin_start(SP_SECTION);
        l.set_margin_end(SP);
        l.set_tooltip_text(Some(tip));
        l.style_context().add_class("dim-label");
        let e = gtk::Entry::new();
        e.set_width_chars(FIELD_CHARS);
        e.set_tooltip_text(Some(tip));
        row.pack_start(&l, false, false, 0);
        row.pack_start(&e, false, false, 0);
        out.push((*f, e));
    }
    let u = gtk::Label::new(Some(unit));
    u.set_margin_start(SP);
    u.style_context().add_class("dim-label");
    row.pack_start(&u, false, false, 0);
    let rotation = rotation.then(|| {
        // A text glyph: icon themes draw this action very differently, and
        // at button size often illegibly.
        let rotate = gtk::Button::new();
        let glyph = gtk::Label::new(None);
        glyph.set_markup("<span size=\"large\">↺</span>");
        rotate.add(&glyph);
        rotate.style_context().add_class("flat");
        rotate.set_tooltip_text(Some("Rotate 90° counter-clockwise"));
        rotate.set_margin_start(SP_SECTION);
        let combo = gtk::ComboBoxText::with_entry();
        for d in ["0°", "90°", "180°", "270°"] {
            combo.append_text(d);
        }
        combo.set_margin_start(SP);
        combo.set_tooltip_text(Some("Rotation, degrees"));
        let e = combo.child().and_then(|c| c.downcast::<gtk::Entry>().ok()).expect("combo with entry");
        e.set_width_chars(8);
        out.push((Field::TR, e));
        row.pack_start(&rotate, false, false, 0);
        row.pack_start(&combo, false, false, 0);
        (rotate, combo)
    });
    FieldRow { row, title: title_button, rotation }
}

/// One line of a side list (Filters tab, plugin manager).
fn list_row(id: &str) -> (gtk::ListBoxRow, gtk::Box) {
    let row = gtk::ListBoxRow::new();
    row.set_widget_name(id);
    let b = gtk::Box::new(gtk::Orientation::Horizontal, SP);
    b.set_margin_start(SP);
    b.set_margin_end(SP);
    b.set_margin_top(SP_TIGHT);
    b.set_margin_bottom(SP_TIGHT);
    row.add(&b);
    (row, b)
}

/// Top of a detail pane: bold name and version, the description, a warning.
fn detail_header(name: &str, version: &str, description: &str, warning: Option<&str>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, SP_TIGHT);
    let title = gtk::Label::new(None);
    title.set_markup(&format!("<b>{}</b>  {}", glib::markup_escape_text(name), glib::markup_escape_text(version)));
    title.set_xalign(0.0);
    b.pack_start(&title, false, false, 0);
    if !description.is_empty() {
        let d = gtk::Label::new(Some(&description.split_whitespace().collect::<Vec<_>>().join(" ")));
        d.set_line_wrap(true);
        d.set_xalign(0.0);
        d.set_max_width_chars(30);
        d.style_context().add_class("dim-label");
        b.pack_start(&d, false, false, 0);
    }
    if let Some(w) = warning {
        let l = gtk::Label::new(Some(w));
        l.set_line_wrap(true);
        l.set_xalign(0.0);
        b.pack_start(&l, false, false, 0);
    }
    b
}

/// The placeholder text of an empty pane.
fn empty_note(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.style_context().add_class("dim-label");
    l.set_xalign(0.0);
    l
}

fn submenu(bar: &gtk::MenuBar, label: &str) -> gtk::Menu {
    let top = gtk::MenuItem::with_mnemonic(label);
    let menu = gtk::Menu::new();
    top.set_submenu(Some(&menu));
    bar.append(&top);
    menu
}

fn item(menu: &gtk::Menu, label: &str, f: impl Fn() + 'static) -> gtk::MenuItem {
    let it = gtk::MenuItem::with_label(label);
    it.connect_activate(move |_| f());
    menu.append(&it);
    it
}

fn check(menu: &gtk::Menu, label: &str, on: bool, radio: bool, f: impl Fn() + 'static) -> gtk::CheckMenuItem {
    let it = gtk::CheckMenuItem::with_label(label);
    it.set_draw_as_radio(radio);
    it.set_active(on);
    it.connect_toggled(move |_| f());
    menu.append(&it);
    it
}

// ---------------------------------------------------------------- rounding

thread_local! {
    static RADIUS: Cell<f64> = const { Cell::new(4.0) };
}

/// Corner radius of the desktop: Hyprland's `decoration:rounding`, 4 px
/// elsewhere. Read once at startup.
fn desktop_rounding() -> f64 {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return 4.0;
    }
    let out = std::process::Command::new("hyprctl").args(["getoption", "decoration:rounding", "-j"]).output();
    let Ok(out) = out else { return 4.0 };
    // {"option": "decoration:rounding", "int": 4, "set": true }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split("\"int\":")
        .nth(1)
        .and_then(|r| r.split([',', '}']).next())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map_or(4.0, |v| v.clamp(0.0, 24.0))
}

/// Rounds the widgets' corners like the compositor rounds windows. Only
/// `border-radius` is touched; everything else stays the GTK theme's. The
/// priority is above the user's gtk.css, which may set `* { border-radius: 0 }`.
fn install_rounding() {
    let r = desktop_rounding();
    RADIUS.with(|c| c.set(r));
    let css = format!(
        "entry, button, combobox button, list row, textview, scrolledwindow, notebook > stack, \
         .view, treeview.view {{ border-radius: {r}px; }}\n\
         notebook > header tab {{ border-radius: {r}px {r}px 0 0; }}"
    );
    add_css(&css);
    add_css(CHROME_CSS);
    install_accent();
}

/// The few rules this window adds on top of the theme:
/// - the menu bar sits in the header line, without its own underline;
/// - the footer's buttons are a bit taller than a text field;
/// - Apply is filled with the theme's accent (see `install_accent`).
const CHROME_CSS: &str = "
menubar.kt-chrome { border: none; box-shadow: none; background: none; }
button.kt-action { min-height: 30px; padding: 0 14px; }
";

/// The accent of the theme as it draws it: the underline of the active tab
/// (falls back to the theme's selection colour). Areas and Apply use it.
fn theme_accent() -> Option<gdk::RGBA> {
    let path = gtk::WidgetPath::new();
    let mut pos = path.append_type(gtk::Notebook::static_type());
    path.iter_set_object_name(pos, Some("notebook"));
    for name in ["header", "tabs", "tab"] {
        pos = path.append_type(gtk::Widget::static_type());
        path.iter_set_object_name(pos, Some(name));
    }
    path.iter_set_state(pos, gtk::StateFlags::CHECKED);
    let sc = gtk::StyleContext::new();
    sc.set_path(&path);
    if let Some(screen) = gdk::Screen::default() {
        sc.set_screen(&screen);
    }
    sc.set_state(gtk::StateFlags::CHECKED);
    let c = sc.style_property_for_state("border-bottom-color", gtk::StateFlags::CHECKED).get::<gdk::RGBA>().ok();
    // A tab without a coloured underline reports its text colour; that is
    // no accent.
    let text = sc.color(gtk::StateFlags::CHECKED);
    c.filter(|c| c.alpha() > 0.0 && hex(c) != hex(&text))
}

thread_local! {
    static ACCENT: Cell<Option<gdk::RGBA>> = const { Cell::new(None) };
}

fn install_accent() {
    let accent = theme_accent().or_else(selection_color);
    ACCENT.with(|a| a.set(accent));
    let Some(c) = accent else { return };
    let luma = 0.2126 * c.red() + 0.7152 * c.green() + 0.0722 * c.blue();
    let on = if luma > 0.55 { "#000000" } else { "#ffffff" };
    let c = hex(&c);
    add_css(&format!(
        "button.kt-primary {{ background-color: {c}; background-image: none; color: {on}; border: none; box-shadow: none; }}\n\
         button.kt-primary:hover {{ background-color: shade({c}, 1.12); color: {on}; }}\n\
         button.kt-primary:active {{ background-color: shade({c}, 0.9); color: {on}; }}"
    ));
}

/// Each sheet is its own provider, so one a theme cannot parse does not
/// take the others with it. Above the user's gtk.css in priority.
fn add_css(css: &str) {
    let provider = gtk::CssProvider::new();
    if provider.load_from_data(css.as_bytes()).is_ok() {
        if let Some(screen) = gdk::Screen::default() {
            gtk::StyleContext::add_provider_for_screen(&screen, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
        }
    }
}

fn radius() -> f64 {
    RADIUS.with(|c| c.get())
}

/// Rectangle path with rounded corners (radius capped to fit).
fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

// ---------------------------------------------------------------- drawing

/// Maps content coordinates (px or mm) into a canvas, preserving aspect.
struct Fit {
    ox: f64,
    oy: f64,
    s: f64,
}

impl Fit {
    fn of(da: &gtk::DrawingArea, w: f64, h: f64) -> Self {
        let (cw, ch) = (da.allocated_width() as f64, da.allocated_height() as f64);
        // The canvas already has the content's aspect ratio (see
        // `output_layout`), so the content fills it edge to edge.
        let margin = 0.0;
        let s = ((cw - 2.0 * margin) / w).min((ch - 2.0 * margin) / h).max(1e-6);
        Self { ox: ((cw - w * s) / 2.0).round(), oy: ((ch - h * s) / 2.0).round(), s }
    }

    fn x(&self, x: f64) -> f64 {
        self.ox + x * self.s
    }

    fn y(&self, y: f64) -> f64 {
        self.oy + y * self.s
    }
}

/// Colours of the area drawings, from the GTK theme — the same system colours
/// OTD's area editor uses (text colour, selection colour).
struct Colors {
    fg: gdk::RGBA,
    /// The theme's accent: area outline, centre dot; a tint of it fills the area.
    accent: gdk::RGBA,
    /// Fill and outline of the monitors / the tablet surface: what the theme
    /// gives a text field, so the previews match the fields under them.
    surface: gdk::RGBA,
    frame: gdk::RGBA,
}

impl Colors {
    fn of(w: &impl IsA<gtk::Widget>) -> Self {
        let sc = w.style_context();
        let fg = sc.color(gtk::StateFlags::NORMAL);
        let accent = ACCENT.with(|a| a.get())
            .or_else(|| sc.lookup_color("theme_selected_bg_color"))
            .or_else(|| sc.lookup_color("selected_bg_color"))
            .or_else(|| sc.lookup_color("accent_bg_color"))
            .unwrap_or_else(|| gdk::RGBA::new(0.21, 0.52, 0.89, 1.0));
        let (surface, frame) = field_colors();
        Self {
            fg,
            accent,
            surface: surface.unwrap_or_else(|| gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), 0.05)),
            frame: frame.unwrap_or_else(|| gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), 0.2)),
        }
    }
}

thread_local! {
    static PROBE_VIEW: gtk::TreeView = gtk::TreeView::new();
    static PROBE_ENTRY: gtk::Entry = gtk::Entry::new();
}

/// Background and border colour of a text field as the theme renders it.
fn field_colors() -> (Option<gdk::RGBA>, Option<gdk::RGBA>) {
    PROBE_ENTRY.with(|e| {
        let sc = e.style_context();
        let get = |prop: &str| {
            sc.style_property_for_state(prop, gtk::StateFlags::NORMAL).get::<gdk::RGBA>().ok().filter(|c| c.alpha() > 0.0)
        };
        (get("background-color"), get("border-top-color"))
    })
}

/// The background of a selected row as the theme actually renders it — what
/// Eto (OTD's toolkit) reports as the system highlight colour. Themes often
/// set it in CSS rules rather than in `theme_selected_bg_color`.
fn selection_color() -> Option<gdk::RGBA> {
    PROBE_VIEW.with(|tv| {
        let sc = tv.style_context();
        sc.save();
        sc.set_state(gtk::StateFlags::SELECTED);
        let v = sc.style_property_for_state("background-color", gtk::StateFlags::SELECTED);
        sc.restore();
        v.get::<gdk::RGBA>().ok().filter(|c| c.alpha() > 0.0)
    })
}

fn source(cr: &cairo::Context, c: &gdk::RGBA, alpha: f64) {
    cr.set_source_rgba(c.red(), c.green(), c.blue(), c.alpha() * alpha);
}

fn text(cr: &cairo::Context, w: &impl IsA<gtk::Widget>, s: &str) -> (f64, f64, pango::Layout) {
    let l = w.create_pango_layout(Some(s));
    // Area labels a bit smaller than the UI text, so they fit small areas.
    let mut font = l.context().font_description().unwrap_or_default();
    font.set_size(font.size() * 85 / 100);
    l.set_font_description(Some(&font));
    let (lw, lh) = l.pixel_size();
    let _ = cr;
    (lw as f64, lh as f64, l)
}

/// Millimetres: two decimals at least, three where they matter ("58.00",
/// "75.225").
fn mm(v: f64) -> String {
    let s = num(v, 3);
    match s.split_once('.') {
        None => format!("{s}.00"),
        Some((_, d)) if d.len() < 2 => format!("{s}0"),
        _ => s,
    }
}

/// Numbers the way OTD prints them: no trailing zeros.
fn num(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Gap around every monitor tile in the Display preview, like a tiling
/// WM's gaps: adjacent monitors read as separate tiles `2 * TILE_GAP` apart.
const TILE_GAP: f64 = 2.0;

/// Draws `l` centred on (x, y), turned by `angle` but never upside down.
fn label_at(cr: &cairo::Context, l: &pango::Layout, (lw, lh): (f64, f64), x: f64, y: f64, angle: f64) {
    let a = if angle.cos() < -1e-6 { angle + PI } else { angle };
    let _ = cr.save();
    cr.translate(x, y);
    cr.rotate(a);
    cr.move_to(-lw / 2.0, -lh / 2.0);
    pangocairo::functions::show_layout(cr, l);
    let _ = cr.restore();
}

/// One area: a tinted, rounded rectangle outlined in the accent, rotated
/// with the area. Its width is written along its top edge, its height along
/// its left edge (both turned with the area but kept readable), the centre
/// dot is in the accent, and the aspect ratio under it turns with the area.
/// Labels that do not fit are left out.
#[allow(clippy::too_many_arguments)]
fn draw_area(cr: &cairo::Context, da: &gtk::DrawingArea, c: &Colors, (cx, cy): (f64, f64), (pw, ph): (f64, f64), angle: f64, [wl, hl, ratio]: [&str; 3]) {
    let _ = cr.save();
    cr.translate(cx, cy);
    cr.rotate(angle);
    rounded(cr, -pw / 2.0 + 0.75, -ph / 2.0 + 0.75, pw - 1.5, ph - 1.5, radius());
    source(cr, &c.accent, 0.16);
    let _ = cr.fill_preserve();
    source(cr, &c.accent, 0.9);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
    source(cr, &c.accent, 1.0);
    cr.arc(0.0, 0.0, 2.0, 0.0, 2.0 * PI);
    let _ = cr.fill();
    source(cr, &c.fg, 0.85);
    let pad = SP as f64;
    let (rw, rh, rl) = text(cr, da, ratio);
    if rw + 2.0 * pad < pw && pad + rh + pad < ph / 2.0 {
        cr.move_to(-rw / 2.0, SP_TIGHT as f64);
        pangocairo::functions::show_layout(cr, &rl);
    }
    let _ = cr.restore();

    // Edge labels, positioned in the area's frame, drawn readable.
    let (sin, cos) = angle.sin_cos();
    let to_screen = |x: f64, y: f64| (cx + x * cos - y * sin, cy + x * sin + y * cos);
    let (ww, wh, wlay) = text(cr, da, wl);
    let (hw, hh, hlay) = text(cr, da, hl);
    let width_fits = ww + 2.0 * pad < pw && wh + pad < ph / 2.0 - rh;
    let height_fits = hw + 2.0 * (wh + pad) < ph && hh + pad < pw / 2.0 - rw / 2.0;
    source(cr, &c.fg, 0.85);
    if width_fits {
        let (x, y) = to_screen(0.0, -ph / 2.0 + pad / 2.0 + wh / 2.0);
        label_at(cr, &wlay, (ww, wh), x, y, angle);
    }
    if height_fits {
        let (x, y) = to_screen(-pw / 2.0 + pad / 2.0 + hh / 2.0, 0.0);
        label_at(cr, &hlay, (hw, hh), x, y, angle - PI / 2.0);
    }
}

/// A monitor or the tablet surface: a tile in the theme's field colours.
fn draw_tile(cr: &cairo::Context, c: &Colors, x: f64, y: f64, w: f64, h: f64) {
    rounded(cr, x.round() + 0.5, y.round() + 0.5, w.round() - 1.0, h.round() - 1.0, radius());
    source(cr, &c.surface, 1.0);
    let _ = cr.fill_preserve();
    source(cr, &c.frame, 1.0);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
}

fn draw_pen(cr: &cairo::Context, c: &Colors, x: f64, y: f64) {
    cr.arc(x, y, 3.0, 0.0, 2.0 * PI);
    source(cr, &c.fg, 1.0);
    let _ = cr.fill();
}

impl App {
    fn draw_display(&self, da: &gtk::DrawingArea, cr: &cairo::Context) {
        let m = self.m.borrow();
        let c = Colors::of(da);
        let fit = Fit::of(da, m.desktop.width, m.desktop.height);
        let g = TILE_GAP;
        let tile = |x: f64, y: f64, w: f64, h: f64| (fit.x(x) + g, fit.y(y) + g, (w * fit.s - 2.0 * g).max(1.0), (h * fit.s - 2.0 * g).max(1.0));
        for mon in &m.desktop.monitors {
            let (x, y, w, h) = tile(mon.x, mon.y, mon.width, mon.height);
            draw_tile(cr, &c, x, y, w, h);
        }
        // The area gets the same gap, so one that covers a monitor sits
        // exactly on that monitor's tile.
        let d = m.cfg.display;
        let (x, y, w, h) = tile(d.x - d.width / 2.0, d.y - d.height / 2.0, d.width, d.height);
        let (wl, hl) = (format!("{} px", num(d.width, 0)), format!("{} px", num(d.height, 0)));
        draw_area(cr, da, &c, (x + w / 2.0, y + h / 2.0), (w, h), 0.0, [&wl, &hl, &num(d.width / d.height, 4)]);
        if let Some((x, y)) = m.pen_px() {
            draw_pen(cr, &c, fit.x(x), fit.y(y));
        }
    }

    fn draw_tablet(&self, da: &gtk::DrawingArea, cr: &cairo::Context) {
        let m = self.m.borrow();
        let c = Colors::of(da);
        let spec = m.probe.spec;
        let fit = Fit::of(da, spec.width_mm, spec.height_mm);
        let (x0, y0, w, h) = (fit.x(0.0), fit.y(0.0), spec.width_mm * fit.s, spec.height_mm * fit.s);
        draw_tile(cr, &c, x0, y0, w, h);
        // Centre lines of the tablet surface and a small cross where they meet.
        let (mx, my) = ((x0 + w / 2.0).round() + 0.5, (y0 + h / 2.0).round() + 0.5);
        source(cr, &c.fg, 0.08);
        cr.set_line_width(1.0);
        cr.move_to(mx, y0 + 1.0);
        cr.line_to(mx, y0 + h - 1.0);
        cr.move_to(x0 + 1.0, my);
        cr.line_to(x0 + w - 1.0, my);
        let _ = cr.stroke();
        source(cr, &c.fg, 0.35);
        cr.move_to(mx - 4.0, my);
        cr.line_to(mx + 4.0, my);
        cr.move_to(mx, my - 4.0);
        cr.line_to(mx, my + 4.0);
        let _ = cr.stroke();

        let t = m.cfg.tablet;
        let k = transform::area_corners(&t);
        let angle = (k[1].1 - k[0].1).atan2(k[1].0 - k[0].0);
        let (wl, hl) = (format!("{} mm", num(t.width, 2)), format!("{} mm", num(t.height, 2)));
        draw_area(
            cr,
            da,
            &c,
            (fit.x(t.x), fit.y(t.y)),
            (t.width * fit.s, t.height * fit.s),
            angle,
            [&wl, &hl, &num(t.width / t.height, 4)],
        );
        if let Some((x, y)) = m.pen_mm() {
            draw_pen(cr, &c, fit.x(x), fit.y(y));
        }
    }

    fn drag_by(&self, da: &gtk::DrawingArea, which: Canvas, dx: f64, dy: f64) {
        {
            let mut m = self.m.borrow_mut();
            let m = &mut *m;
            match which {
                Canvas::Display => {
                    let s = Fit::of(da, m.desktop.width, m.desktop.height).s;
                    m.cfg.display.x += dx / s;
                    m.cfg.display.y += dy / s;
                    clamp_display(&mut m.cfg.display, &m.desktop);
                }
                Canvas::Tablet => {
                    let spec = m.probe.spec;
                    let s = Fit::of(da, spec.width_mm, spec.height_mm).s;
                    m.cfg.tablet.x += dx / s;
                    m.cfg.tablet.y += dy / s;
                    clamp_tablet(&mut m.cfg.tablet, spec);
                }
            }
        }
        self.sync();
    }

    fn context_menu(self: &Rc<Self>, da: &gtk::DrawingArea, which: Canvas, ev: &gdk::EventButton) {
        let menu = self.area_menu(which);
        menu.set_attach_widget(Some(da));
        menu.popup_at_pointer(Some(ev));
        *self.popup.borrow_mut() = Some(menu);
    }

    /// The same menu under a row's "Display ▾" / "Tablet ▾" button.
    fn menu_under(self: &Rc<Self>, button: &gtk::Button, which: Canvas) {
        let menu = self.area_menu(which);
        menu.set_attach_widget(Some(button));
        menu.popup_at_widget(button, gdk::Gravity::SouthWest, gdk::Gravity::NorthWest, None);
        *self.popup.borrow_mut() = Some(menu);
    }

    fn area_menu(self: &Rc<Self>, which: Canvas) -> gtk::Menu {
        let menu = gtk::Menu::new();
        let edit = |f: Box<dyn Fn(&mut Model)>| {
            let app = Rc::downgrade(self);
            move || {
                if let Some(app) = app.upgrade() {
                    f(&mut app.m.borrow_mut());
                    app.sync();
                }
            }
        };
        match which {
            Canvas::Display => {
                let m = self.m.borrow();
                for mon in &m.desktop.monitors {
                    let area = DisplayArea {
                        width: mon.width,
                        height: mon.height,
                        x: mon.x + mon.width / 2.0,
                        y: mon.y + mon.height / 2.0,
                    };
                    item(&menu, &format!("Map to {} ({:.0}×{:.0})", mon.name, mon.width, mon.height), edit(Box::new(move |m| {
                        m.cfg.display = area;
                        m.relock(true);
                    })));
                }
                let whole = DisplayArea {
                    width: m.desktop.width,
                    height: m.desktop.height,
                    x: m.desktop.width / 2.0,
                    y: m.desktop.height / 2.0,
                };
                item(&menu, "Map to the whole desktop", edit(Box::new(move |m| {
                    m.cfg.display = whole;
                    m.relock(true);
                })));
                menu.append(&gtk::SeparatorMenuItem::new());
                let bind = gtk::Menu::new();
                let backend = m.desktop.backend;
                for mode in BindMode::ALL {
                    let (label, tip) = match mode {
                        BindMode::Auto if backend.can_bind() => (
                            "Automatic",
                            format!(
                                "kerntablet binds the tablet to the monitor through {}'s own interface. Your compositor config is never edited.",
                                backend.label()
                            ),
                        ),
                        BindMode::Auto => (
                            "Automatic",
                            format!("Not available on {}: there is no known interface to bind a tablet from outside. Use manual.", backend.label()),
                        ),
                        BindMode::Manual => (
                            "Manual",
                            "You bound the tablet to the monitor in your compositor's settings yourself; kerntablet only computes the area for that monitor.".into(),
                        ),
                        BindMode::Off => ("Off", "The tablet covers the whole desktop; the area is cut out of it.".into()),
                    };
                    check(&bind, label, m.cfg.bind_output == mode, true, edit(Box::new(move |m| m.cfg.bind_output = mode)))
                        .set_tooltip_text(Some(&tip));
                }
                bind.append(&gtk::SeparatorMenuItem::new());
                item(&bind, "How to keep the binding…", glib::clone!(@weak self as app => move || app.explain_binding()));
                let top = gtk::MenuItem::with_label("Monitor binding");
                top.set_tooltip_text(Some(
                    "When the screen area lies on one monitor, the tablet can be bound to that monitor in the compositor \
                     (like OTD's output setting). Needed for osu! under Wine.",
                ));
                top.set_submenu(Some(&bind));
                menu.append(&top);
            }
            Canvas::Tablet => {
                let m = self.m.borrow();
                item(&menu, "Full area", edit(Box::new(|m| {
                    let spec = m.probe.spec;
                    let rot = m.cfg.tablet.rotation;
                    m.cfg.tablet = full_tablet(spec);
                    m.cfg.tablet.rotation = rot;
                    if is_quarter_turn(rot) {
                        std::mem::swap(&mut m.cfg.tablet.width, &mut m.cfg.tablet.height);
                    }
                })));
                item(&menu, "Match screen aspect ratio", edit(Box::new(|m| {
                    let (d, spec) = (m.cfg.display, m.probe.spec);
                    match_aspect(&mut m.cfg.tablet, &d, spec, true);
                    clamp_tablet(&mut m.cfg.tablet, spec);
                })));
                item(&menu, "Center", edit(Box::new(|m| {
                    m.cfg.tablet.x = m.probe.spec.width_mm / 2.0;
                    m.cfg.tablet.y = m.probe.spec.height_mm / 2.0;
                })));
                let rot = gtk::Menu::new();
                for deg in [0.0, 90.0, 180.0, 270.0] {
                    check(&rot, &format!("{deg:.0}°"), m.cfg.tablet.rotation == deg, true, edit(Box::new(move |m| {
                        m.cfg.tablet.rotation = deg;
                        let spec = m.probe.spec;
                        clamp_tablet(&mut m.cfg.tablet, spec);
                    })));
                }
                let top = gtk::MenuItem::with_label("Rotation");
                top.set_submenu(Some(&rot));
                menu.append(&top);
                menu.append(&gtk::SeparatorMenuItem::new());
                check(&menu, "Lock aspect ratio", m.cfg.lock_aspect, false, edit(Box::new(|m| {
                    m.cfg.lock_aspect = !m.cfg.lock_aspect;
                    m.relock(true);
                })))
                .set_tooltip_text(Some("Keep the tablet area's aspect ratio equal to the screen area's"));
                check(&menu, "Clip cursor to the screen area", m.cfg.clip, false, edit(Box::new(|m| m.cfg.clip = !m.cfg.clip)))
                    .set_tooltip_text(Some("Past the edge of the tablet area the cursor stops at the edge of the screen area"));
                check(&menu, "Ignore input outside the area", m.cfg.limit, false, edit(Box::new(|m| m.cfg.limit = !m.cfg.limit)))
                    .set_tooltip_text(Some("Past the edge of the tablet area the pen is treated as out of range"));
            }
        }
        menu.show_all();
        menu
    }

    fn connect_rows(self: &Rc<Self>, display: &FieldRow, tablet: &FieldRow) {
        display.title.connect_clicked(glib::clone!(@weak self as app => move |b| app.menu_under(b, Canvas::Display)));
        tablet.title.connect_clicked(glib::clone!(@weak self as app => move |b| app.menu_under(b, Canvas::Tablet)));
        if let Some((rotate, combo)) = &tablet.rotation {
            rotate.connect_clicked(glib::clone!(@weak self as app => move |_| {
                {
                    let mut m = app.m.borrow_mut();
                    let spec = m.probe.spec;
                    let t = &mut m.cfg.tablet;
                    t.rotation = (t.rotation + 270.0).rem_euclid(360.0);
                    clamp_tablet(t, spec);
                }
                app.sync();
            }));
            // Picking from the list (typing is handled like any field).
            combo.connect_changed(glib::clone!(@weak self as app => move |c| {
                if c.active().is_some() {
                    if let Some(t) = c.active_text() {
                        app.commit(Field::TR, &t);
                    }
                }
            }));
        }
    }

    fn explain_binding(&self) {
        let d = gtk::MessageDialog::new(
            Some(&self.ui.window),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Info,
            gtk::ButtonsType::Close,
            "Keeping the monitor binding",
        );
        d.set_secondary_text(Some(
            "In automatic mode kerntablet binds the tablet at runtime, and some compositors (Hyprland, sway) forget \
             that binding when they reload their config. kerntablet re-applies it at every login.\n\n\
             To make it permanent, source the snippet kerntablet keeps for you from your compositor config:\n\
             ~/.config/kerntablet/hyprland.conf (or hyprland.lua, sway.conf).",
        ));
        d.run();
        unsafe { d.destroy() };
    }
}

// ---------------------------------------------------------------- output layout

/// A widget's place inside the Output tab.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Place {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputLayout {
    display: Place,
    display_row: Place,
    tablet: Place,
    tablet_row: Place,
}

const PAD: i32 = SP_SECTION;
/// Between a preview and its row of fields.
const ROW_GAP: i32 = SP_SECTION;
/// Between the Display block and the Tablet block.
const BLOCK_GAP: i32 = SP_SECTION;
/// The smallest preview that is still usable.
const MIN_PREVIEW: i32 = 48;
/// The tablet preview is at most this share of the Display preview's width,
/// so the monitors stay the widest element.
const TABLET_SCALE: f64 = 0.8;

/// Height of everything but the two previews (row heights given).
fn fixed_height(d_row: i32, t_row: i32) -> i32 {
    2 * PAD + d_row + t_row + 2 * ROW_GAP + BLOCK_GAP
}

/// Where everything on the Output tab goes for a `w`×`h` tab.
///
/// Top to bottom on one centre axis: monitors, their row of fields, the
/// tablet, its row; anchored at the top. Each preview has exactly the
/// aspect ratio of what it shows. The Display preview takes the full width
/// if the height allows, the tablet preview `TABLET_SCALE` of that; if the
/// height does not allow it, both shrink together. Below the minimum size
/// (only a tiling WM can force that) previews stay at `MIN_PREVIEW` and the
/// bottom is clipped. `d_row`/`t_row` are the rows' natural sizes.
fn output_layout(w: i32, h: i32, desktop_aspect: f64, tablet_aspect: f64, d_row: (i32, i32), t_row: (i32, i32)) -> OutputLayout {
    let inner_w = (w - 2 * PAD).max(1) as f64;
    let room = (h - fixed_height(d_row.1, t_row.1)).max(0) as f64;
    let (da, ta) = (desktop_aspect.max(0.05), tablet_aspect.max(0.05));
    // wd/da + TABLET_SCALE*wd/ta <= room
    let wd = inner_w.min(room / (1.0 / da + TABLET_SCALE / ta)).max(1.0);
    let wt = wd * TABLET_SCALE;
    let size = |pw: f64, aspect: f64| {
        let ph = ((pw / aspect).round() as i32).max(MIN_PREVIEW);
        let pw = if (ph as f64) > pw / aspect + 0.5 { ph as f64 * aspect } else { pw };
        ((pw.round() as i32).clamp(1, inner_w as i32), ph)
    };
    let ((pw_d, ph_d), (pw_t, ph_t)) = (size(wd, da), size(wt, ta));

    let centre = |cw: i32| PAD.max((w - cw) / 2);
    let mut y = PAD;
    let mut next = |cw: i32, ch: i32, gap: i32| {
        let p = Place { x: centre(cw), y, w: cw, h: ch };
        y += ch + gap;
        p
    };
    OutputLayout {
        display: next(pw_d, ph_d, ROW_GAP),
        display_row: next(d_row.0, d_row.1, BLOCK_GAP),
        tablet: next(pw_t, ph_t, ROW_GAP),
        tablet_row: next(t_row.0, t_row.1, 0),
    }
}

impl App {
    fn row_sizes(&self) -> ((i32, i32), (i32, i32)) {
        let nat = |r: &gtk::Box| (r.preferred_width().1, r.preferred_height().1);
        (nat(&self.ui.display_row), nat(&self.ui.tablet_row))
    }

    /// Smallest Output tab that shows everything: the wider row of fields,
    /// and both blocks with minimal previews.
    fn output_min_size(&self) -> (i32, i32) {
        let (d, t) = self.row_sizes();
        (d.0.max(t.0) + 2 * PAD, fixed_height(d.1, t.1) + 2 * MIN_PREVIEW)
    }

    fn update_output_min(&self) {
        let (w, h) = self.output_min_size();
        if self.ui.output.size_request() != (w, h) {
            self.ui.output.set_size_request(w, h);
        }
    }

    /// Places the previews and rows for the tab's current allocation. Runs
    /// on every size-allocate of the tab (window resize, WM configure,
    /// `queue_resize` after the monitor layout or the tablet changed), right
    /// after GTK's own placement, so the first frame is already laid out.
    fn relayout(&self, a: &gtk::Allocation) {
        let (desktop_aspect, tablet_aspect) = {
            let m = self.m.borrow();
            (m.desktop.width / m.desktop.height.max(1.0), m.probe.spec.width_mm / m.probe.spec.height_mm)
        };
        let (d, t) = self.row_sizes();
        let l = output_layout(a.width(), a.height(), desktop_aspect, tablet_aspect, d, t);
        // gtk::Fixed has no window of its own: child allocations are in the
        // parent's coordinates.
        let put = |w: &gtk::Widget, p: Place| {
            let _ = (w.preferred_width(), w.preferred_height());
            w.size_allocate(&gtk::Allocation::new(a.x() + p.x, a.y() + p.y, p.w, p.h));
        };
        put(self.ui.display.upcast_ref(), l.display);
        put(self.ui.display_row.upcast_ref(), l.display_row);
        put(self.ui.tablet.upcast_ref(), l.tablet);
        put(self.ui.tablet_row.upcast_ref(), l.tablet_row);
    }
}

// ---------------------------------------------------------------- model

impl Model {
    fn pen_mm(&self) -> Option<(f64, f64)> {
        let p = self.pen.filter(|p| p.in_prox != 0)?;
        let s = self.probe.spec;
        Some((p.raw_x as f64 * s.width_mm / s.max_x as f64, p.raw_y as f64 * s.height_mm / s.max_y as f64))
    }

    /// Where the cursor would be with the settings currently in the editor.
    fn pen_px(&self) -> Option<(f64, f64)> {
        let p = self.pen.filter(|p| p.in_prox != 0)?;
        let s = self.probe.spec;
        let t = self.desktop.target_for(&self.cfg);
        let k = transform::compute_for(s, &self.cfg, &t);
        let (ox, oy) = k.apply(p.raw_x.min(u16::MAX as u32) as u16, p.raw_y.min(u16::MAX as u32) as u16)?;
        Some((t.x + ox as f64 * t.width / (s.max_x as f64 + 1.0), t.y + oy as f64 * t.height / (s.max_y as f64 + 1.0)))
    }

    /// Re-applies "lock aspect ratio" after an edit.
    fn relock(&mut self, keep_width: bool) {
        if self.cfg.lock_aspect {
            let d = self.cfg.display;
            match_aspect(&mut self.cfg.tablet, &d, self.probe.spec, keep_width);
        }
    }
}

impl App {
    /// Fills the entries from the model and redraws the areas.
    fn sync(&self) {
        self.updating.set(true);
        {
            let m = self.m.borrow();
            let (d, t) = (m.cfg.display, m.cfg.tablet);
            for (f, e) in &self.ui.entries {
                let s = match f {
                    Field::DW => num(d.width, 2),
                    Field::DH => num(d.height, 2),
                    Field::DX => num(d.x, 2),
                    Field::DY => num(d.y, 2),
                    Field::TW => mm(t.width),
                    Field::TH => mm(t.height),
                    Field::TX => mm(t.x),
                    Field::TY => mm(t.y),
                    Field::TR => format!("{}°", num(t.rotation, 2)),
                };
                if e.text() != s {
                    e.set_text(&s);
                }
            }
        }
        self.updating.set(false);
        self.ui.display.queue_draw();
        self.ui.tablet.queue_draw();
        self.update_dirty();
    }

    fn commit(&self, field: Field, text: &str) {
        if self.updating.get() {
            return;
        }
        let Ok(v) = text.trim().trim_end_matches('°').trim().replace(',', ".").parse::<f64>() else {
            self.sync();
            return;
        };
        {
            let mut m = self.m.borrow_mut();
            let c = &mut m.cfg;
            let slot = match field {
                Field::DW => &mut c.display.width,
                Field::DH => &mut c.display.height,
                Field::DX => &mut c.display.x,
                Field::DY => &mut c.display.y,
                Field::TW => &mut c.tablet.width,
                Field::TH => &mut c.tablet.height,
                Field::TX => &mut c.tablet.x,
                Field::TY => &mut c.tablet.y,
                Field::TR => &mut c.tablet.rotation,
            };
            let v = match field {
                Field::DW | Field::DH | Field::TW | Field::TH => v.max(1.0),
                _ => v,
            };
            if (*slot - v).abs() < 1e-9 {
                drop(m);
                self.sync();
                return;
            }
            *slot = v;
            m.relock(field != Field::TH);
        }
        self.sync();
    }

    fn log(&self, msg: &str) {
        let time = glib::DateTime::now_local().ok().and_then(|t| t.format("%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
        let b = &self.ui.console;
        b.insert(&mut b.end_iter(), &format!("[{time}] {msg}\n"));
    }

    /// Short line in the footer; the full text goes to the Console.
    fn note(&self, msg: &str, error: bool) {
        self.log(msg);
        let short = if error { msg.to_string() } else { msg.split(" (").next().unwrap_or(msg).to_string() };
        let markup = glib::markup_escape_text(&short);
        if error {
            let c = self.ui.message.style_context().lookup_color("error_color");
            self.ui.message.set_markup(&match c {
                Some(c) => format!("<span foreground=\"{}\">{markup}</span>", hex(&c)),
                None => markup.to_string(),
            });
        } else {
            self.ui.message.set_markup(&markup);
        }
        self.ui.message.set_tooltip_text(Some(msg));
    }

    /// Applies and saves: from now on this is what the tablet gets on every
    /// replug and login.
    fn apply(self: &Rc<Self>) {
        let (area, filters) = {
            let mut m = self.m.borrow_mut();
            m.probe = core::probe(m.cfg.preferred);
            let area = core::apply(&m.cfg, &m.probe, &m.desktop, true);
            let filters = core::apply_plugins(&m.psettings, &m.plugins, &m.probe, true);
            if area.is_ok() {
                m.baseline = m.cfg;
            }
            if filters.is_ok() {
                m.pbaseline = m.psettings.clone();
            }
            (area, filters)
        };
        match (area, filters) {
            (Ok(msg), Ok(extra)) => {
                let msg = msg + &extra;
                self.log(&msg);
                let extra = if msg.contains(" (") { " — see Console" } else { "" };
                self.ui.message.set_text(&format!("Applied{extra}"));
                self.ui.message.set_tooltip_text(Some(&msg));
            }
            (Ok(msg), Err(e)) => self.note(&format!("{msg}; filters: {e}"), true),
            (Err(e), _) => self.note(&e, true),
        }
        self.refresh_status();
        self.update_dirty();
    }

    /// Back to what was last applied.
    fn discard(self: &Rc<Self>) {
        {
            let mut m = self.m.borrow_mut();
            m.cfg = m.baseline;
            m.psettings = m.pbaseline.clone();
            if m.probe.device.is_some() && m.cfg.preferred.is_some() {
                m.probe = core::probe(m.cfg.preferred);
            }
        }
        self.ui.output.queue_resize();
        self.note("Changes discarded", false);
        self.sync();
        self.rebuild_filters();
        self.refresh_status();
    }

    fn update_dirty(&self) {
        let dirty = {
            let m = self.m.borrow();
            m.cfg != m.baseline || m.psettings != m.pbaseline
        };
        self.ui.discard.set_sensitive(dirty);
    }

    fn import_otd(self: &Rc<Self>) {
        let ok = {
            let mut m = self.m.borrow_mut();
            match config::import_otd() {
                Some(mut c) => {
                    c.sanitize(m.probe.spec);
                    m.cfg = c;
                    if let Some(otd) = config::read_otd() {
                        let m = &mut *m;
                        for p in &m.plugins {
                            if let Some(e) = p.import_otd(&otd) {
                                m.psettings.plugin.insert(p.id().to_string(), e);
                            }
                        }
                    }
                    true
                }
                None => false,
            }
        };
        if ok {
            self.note("Settings imported from OpenTabletDriver — press Apply", false);
        } else {
            self.note("OpenTabletDriver profile not found (~/.config/OpenTabletDriver/settings.json)", true);
        }
        self.sync();
        self.rebuild_filters();
    }

    fn redetect(self: &Rc<Self>) {
        {
            let mut m = self.m.borrow_mut();
            m.probe = core::probe(m.cfg.preferred);
            m.last_probe = Instant::now();
        }
        self.refresh_status();
        self.log_status();
    }

    fn log_status(&self) {
        let lines = {
            let m = self.m.borrow();
            let mut l = vec![match &m.probe.device {
                Some(d) => format!("Tablet: {} ({}, driver {})", m.probe.spec.name, d.name, d.driver.as_deref().unwrap_or("none")),
                None => format!(
                    "No supported tablet connected. Supported: {}",
                    TABLETS.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
                ),
            }];
            l.push(format!("Status: {}", m.probe.health.describe(m.probe.spec)));
            if let Some(p) = m.pen {
                l.push(format!("Pen reports processed in kernel: {}", p.events));
            }
            l.push(format!("Session: {} — desktop {:.0}×{:.0}", m.desktop.backend.label(), m.desktop.width, m.desktop.height));
            for mon in &m.desktop.monitors {
                l.push(format!("  {} {:.0}×{:.0} @ {:.0},{:.0}", mon.name, mon.width, mon.height, mon.x, mon.y));
            }
            if let Some(e) = &m.desktop_error {
                l.push(format!("Monitors: {e}"));
            }
            l
        };
        for l in lines {
            self.log(&l);
        }
    }

    /// The tablet picker and the health dot in the footer.
    fn refresh_status(self: &Rc<Self>) {
        let (names, current, health, describe) = {
            let m = self.m.borrow();
            let names: Vec<(String, String)> =
                m.probe.connected.iter().map(|(d, s)| (d.name.clone(), short_name(s.name))).collect();
            let current = m.probe.device.as_ref().map(|d| d.name.clone());
            (names, current, m.probe.health.clone(), m.probe.health.describe(m.probe.spec))
        };
        let tablet_name = names.iter().find(|(d, _)| Some(d) == current.as_ref()).map(|(_, n)| n.clone());
        // With several tablets plugged in, the Tablets menu picks one.
        self.updating.set(true);
        let keys: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        let mut items = self.ui_items();
        if items.0 != keys {
            for it in items.1.drain(..) {
                self.ui.tablets_menu.remove(&it);
            }
            for (dev, name) in names.iter().filter(|_| names.len() > 1) {
                let it = gtk::CheckMenuItem::with_label(&format!("{name}  ({dev})"));
                it.set_draw_as_radio(true);
                let dev = dev.clone();
                let app = Rc::downgrade(self);
                it.connect_toggled(move |_| {
                    if let Some(app) = app.upgrade() {
                        app.tablet_picked(&dev);
                    }
                });
                self.ui.tablets_menu.append(&it);
                it.show();
                items.1.push(it);
            }
            items.0 = keys;
        }
        for (it, (dev, _)) in items.1.iter().zip(&names) {
            it.set_active(Some(dev) == current.as_ref());
        }
        drop(items);
        self.updating.set(false);

        self.ui.tablet_name.set_text(tablet_name.as_deref().unwrap_or("No tablet"));
        let (state, color) = match health {
            Health::Active => ("Active", "success_color"),
            Health::NoTablet => ("Not connected", "insensitive_fg_color"),
            Health::WrongDriver(_) => ("Wrong driver", "error_color"),
            Health::NotAttached => ("Not loaded", "error_color"),
            Health::NoAccess(_) => ("No access", "error_color"),
        };
        let sc = self.ui.status.style_context();
        let dot = match sc.lookup_color(color) {
            Some(c) => format!("<span foreground=\"{}\">●</span>", hex(&c)),
            None => "●".into(),
        };
        self.ui.status.set_markup(&format!("{dot}  {state}"));
        self.ui.status.set_tooltip_text(Some(&describe));
    }

    fn ui_items(&self) -> std::cell::RefMut<'_, (Vec<String>, Vec<gtk::CheckMenuItem>)> {
        self.tablet_items.borrow_mut()
    }

    fn tablet_picked(self: &Rc<Self>, dev: &str) {
        if self.updating.get() {
            return;
        }
        let switched = {
            let mut m = self.m.borrow_mut();
            let Some(id) = m.probe.connected.iter().find(|(d, _)| d.name == dev).map(|(_, s)| TabletId::of(s)) else {
                return;
            };
            if m.probe.device.as_ref().is_some_and(|d| d.name == dev) {
                return;
            }
            m.cfg.preferred = Some(id);
            m.probe = core::probe(m.cfg.preferred);
            let spec = m.probe.spec;
            m.cfg.sanitize(spec);
            m.reader = None;
            spec.name
        };
        self.ui.output.queue_resize();
        self.note(&format!("Switched to {switched} — check the area and press Apply"), false);
        self.refresh_status();
        self.sync();
    }

    /// Called every 16 ms: live pen, background jobs, hotplug, monitor changes.
    fn tick(self: &Rc<Self>) {
        self.poll_job();
        let now = Instant::now();
        let (probe_due, desktop_due) = {
            let m = self.m.borrow();
            (
                now.duration_since(m.last_probe) >= Duration::from_secs(1),
                now.duration_since(m.last_desktop) >= Duration::from_secs(3) && self.drag.get().is_none(),
            )
        };
        if probe_due {
            {
                let mut m = self.m.borrow_mut();
                m.last_probe = now;
                m.probe = core::probe(m.cfg.preferred);
            }
            self.refresh_status();
        }
        if desktop_due {
            let changed = {
                let mut m = self.m.borrow_mut();
                m.last_desktop = now;
                match desktop::detect() {
                    Ok(d) => {
                        let changed = d != m.desktop && m.desktop_error.is_none();
                        m.desktop = d;
                        m.desktop_error = None;
                        changed
                    }
                    Err(e) => {
                        m.desktop_error = Some(e);
                        false
                    }
                }
            };
            if changed {
                // The desktop's aspect ratio decides the previews' sizes.
                self.ui.output.queue_resize();
                self.note("Monitor layout changed — check the area and press Apply", false);
                self.sync();
            }
        }

        // Live pen position straight from the BPF program's state map.
        let moved = {
            let mut m = self.m.borrow_mut();
            let dev = m.probe.device.as_ref().filter(|_| m.probe.health.is_active()).map(|d| d.name.clone());
            match dev {
                Some(name) => {
                    if m.reader.as_ref().map(|(n, _)| n.as_str()) != Some(name.as_str()) {
                        m.reader = StateReader::open(&name).ok().map(|r| (name, r));
                    }
                }
                None => m.reader = None,
            }
            let pen = m.reader.as_ref().and_then(|(_, r)| r.read().ok());
            let key = |p: &Option<KtState>| p.map(|p| (p.raw_x, p.raw_y, p.in_prox));
            let moved = key(&pen) != key(&m.pen);
            m.pen = pen;
            moved
        };
        if moved {
            self.ui.display.queue_draw();
            self.ui.tablet.queue_draw();
        }
    }

    // ------------------------------------------------------------ jobs

    fn start_job(self: &Rc<Self>, label: String, f: impl FnOnce() -> JobResult + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        self.log(&label);
        self.m.borrow_mut().job = Some((label, rx));
        self.refresh_manager();
    }

    fn poll_job(self: &Rc<Self>) {
        let res = {
            let mut m = self.m.borrow_mut();
            let Some((_, rx)) = &m.job else { return };
            let Ok(res) = rx.try_recv() else { return };
            m.job = None;
            res
        };
        match res {
            JobResult::Index(r) => {
                if let Err(e) = &r {
                    self.log(&format!("Plugin repository: {e}"));
                }
                self.m.borrow_mut().index = Some(r);
            }
            JobResult::Done(r) => {
                self.rescan_plugins();
                {
                    let mut m = self.m.borrow_mut();
                    m.probe = core::probe(m.cfg.preferred);
                }
                match r {
                    Ok(msg) => {
                        let first = msg.lines().find(|l| l.contains("plugin")).unwrap_or("Done").to_string();
                        self.note(&format!("{first} — press Apply to send its settings"), false);
                    }
                    Err(e) => self.note(&e, true),
                }
                self.rebuild_filters();
            }
        }
        self.refresh_manager();
    }

    fn rescan_plugins(&self) {
        let errors = {
            let mut m = self.m.borrow_mut();
            let (plugins, errors) = plugin::installed();
            m.plugins = plugins;
            m.plugin_errors = errors.clone();
            let m = &mut *m;
            m.psettings.normalize(&m.plugins);
            errors
        };
        for e in errors {
            self.log(&format!("Broken plugin {e}"));
        }
    }

    // ------------------------------------------------------------ filters tab

    fn rebuild_filters(self: &Rc<Self>) {
        self.updating.set(true);
        let list = &self.ui.filter_list;
        for c in list.children() {
            list.remove(&c);
        }
        let (rows, sel) = {
            let mut m = self.m.borrow_mut();
            let rows: Vec<(String, String, bool)> = m
                .plugins
                .iter()
                .map(|p| (p.id().to_string(), p.manifest.name.clone(), m.psettings.plugin.get(p.id()).is_some_and(|e| e.enabled)))
                .collect();
            if m.filter_sel.as_ref().is_none_or(|s| !rows.iter().any(|r| &r.0 == s)) {
                m.filter_sel = rows.first().map(|r| r.0.clone());
            }
            (rows, m.filter_sel.clone())
        };
        for (id, name, on) in rows {
            let (row, b) = list_row(&id);
            let cb = gtk::CheckButton::new();
            cb.set_active(on);
            let pid = id.clone();
            cb.connect_toggled(glib::clone!(@weak self as app => move |cb| {
                if app.updating.get() {
                    return;
                }
                if let Some(e) = app.m.borrow_mut().psettings.plugin.get_mut(&pid) {
                    e.enabled = cb.is_active();
                }
                app.rebuild_filter_detail();
            }));
            let l = gtk::Label::new(Some(&name));
            l.set_xalign(0.0);
            b.pack_start(&cb, false, false, 0);
            b.pack_start(&l, true, true, 0);
            list.add(&row);
            if sel.as_deref() == Some(id.as_str()) {
                list.select_row(Some(&row));
            }
        }
        list.show_all();
        self.updating.set(false);
        self.rebuild_filter_detail();
    }

    fn rebuild_filter_detail(self: &Rc<Self>) {
        let bx = &self.ui.filter_detail;
        for c in bx.children() {
            bx.remove(&c);
        }
        let m = self.m.borrow();
        let Some(p) = m.filter_sel.as_ref().and_then(|id| m.plugins.iter().find(|p| p.id() == id)) else {
            let l = empty_note(if m.plugins.is_empty() {
                "No filters installed.\nPlugins → Plugin manager installs them."
            } else {
                "Select a filter on the left."
            });
            bx.pack_start(&l, false, false, 0);
            bx.show_all();
            return;
        };
        let entry = m.psettings.plugin.get(p.id()).cloned().unwrap_or_else(|| p.resolve(None));
        let man = &p.manifest;
        // Three sections: header, parameters, actions.
        let warning = m
            .probe
            .device
            .as_ref()
            .filter(|dev| !bpfmap::plugin_attached(&dev.name, p.id()))
            .map(|_| "Not loaded for this tablet — replug it or reinstall the plugin");
        bx.pack_start(&detail_header(&man.name, &man.version, &man.description, warning), false, false, 0);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(SP_TIGHT as u32);
        grid.set_column_spacing(SP as u32);
        grid.set_margin_top(SP_SECTION);
        grid.set_sensitive(entry.enabled);
        for (row, param) in man.params.iter().enumerate() {
            let row = row as i32;
            let active = param.enabled_if.as_ref().is_none_or(|k| entry.values.get(k).is_some_and(|v| v.as_bool()));
            let v = entry.values.get(&param.key).copied().unwrap_or(param.default);
            let label = gtk::Label::new(Some(&param.label));
            label.set_xalign(0.0);
            label.set_sensitive(active);
            label.set_tooltip_text(param.tip.as_deref());
            grid.attach(&label, 0, row, 1, 1);
            let (pid, key) = (p.id().to_string(), param.key.clone());
            match param.kind {
                ParamKind::Bool => {
                    let cb = gtk::CheckButton::new();
                    cb.set_active(v.as_bool());
                    cb.set_sensitive(active);
                    cb.set_tooltip_text(param.tip.as_deref());
                    cb.connect_toggled(glib::clone!(@weak self as app => move |cb| {
                        if let Some(e) = app.m.borrow_mut().psettings.plugin.get_mut(&pid) {
                            e.values.insert(key.clone(), Value::Bool(cb.is_active()));
                        }
                        // Other params may depend on this one.
                        let app2 = app.clone();
                        glib::idle_add_local_once(move || app2.rebuild_filter_detail());
                    }));
                    grid.attach(&cb, 1, row, 1, 1);
                }
                ParamKind::Float => {
                    let e = gtk::Entry::new();
                    e.set_width_chars(FIELD_CHARS);
                    e.set_text(&num(v.as_f64(), 4));
                    e.set_sensitive(active);
                    e.set_tooltip_text(param.tip.as_deref());
                    let (min, max) = (param.min.unwrap_or(f64::MIN), param.max.unwrap_or(f64::MAX));
                    let commit = glib::clone!(@weak self as app => move |e: &gtk::Entry| {
                        let Ok(n) = e.text().trim().replace(',', ".").parse::<f64>() else { return };
                        let n = n.clamp(min, max);
                        if let Some(en) = app.m.borrow_mut().psettings.plugin.get_mut(&pid) {
                            en.values.insert(key.clone(), Value::Num(n));
                        }
                        e.set_text(&num(n, 4));
                        app.update_dirty();
                    });
                    let c2 = commit.clone();
                    e.connect_activate(move |e| c2(e));
                    e.connect_focus_out_event(move |e, _| {
                        commit(e);
                        glib::Propagation::Proceed
                    });
                    let cell = gtk::Box::new(gtk::Orientation::Horizontal, SP_TIGHT);
                    cell.pack_start(&e, false, false, 0);
                    if let Some(u) = &param.unit {
                        let l = gtk::Label::new(Some(u));
                        l.set_sensitive(active);
                        cell.pack_start(&l, false, false, 0);
                    }
                    grid.attach(&cell, 1, row, 1, 1);
                }
            }
        }
        bx.pack_start(&grid, false, false, 0);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, SP);
        buttons.set_margin_top(SP_SECTION);
        let defaults = gtk::Button::with_label("Defaults");
        defaults.set_tooltip_text(Some("Reset the parameters to the plugin's defaults"));
        let pid = p.id().to_string();
        defaults.connect_clicked(glib::clone!(@weak self as app => move |_| {
            {
                let mut m = app.m.borrow_mut();
                let Some(def) = m.plugins.iter().find(|p| p.id() == pid).map(|p| p.resolve(None)) else { return };
                if let Some(e) = m.psettings.plugin.get_mut(&pid) {
                    *e = PluginEntry { enabled: e.enabled, ..def };
                }
            }
            app.rebuild_filter_detail();
        }));
        buttons.pack_start(&defaults, false, false, 0);
        let info = [man.license.clone(), man.homepage.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        if !info.is_empty() {
            let l = gtk::Label::new(Some(&info));
            l.style_context().add_class("dim-label");
            l.set_ellipsize(pango::EllipsizeMode::End);
            l.set_xalign(0.0);
            l.set_tooltip_text(Some(&info));
            buttons.pack_start(&l, true, true, 0);
        }
        bx.pack_start(&buttons, false, false, 0);
        bx.show_all();
        drop(m);
        self.update_dirty();
    }

    // ------------------------------------------------------------ plugin manager

    fn open_manager(self: &Rc<Self>) {
        if self.manager.borrow().is_none() {
            let window = gtk::Window::new(gtk::WindowType::Toplevel);
            window.set_title("Plugin Manager");
            window.set_transient_for(Some(&self.ui.window));
            window.set_default_size(520, 320);
            window.connect_delete_event(|w, _| {
                w.hide();
                glib::Propagation::Stop
            });
            let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
            let list = gtk::ListBox::new();
            let left = gtk::ScrolledWindow::new(gtk::Adjustment::NONE, gtk::Adjustment::NONE);
            left.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
            left.set_size_request(SIDE_LIST, -1);
            left.add(&list);
            let detail = gtk::Box::new(gtk::Orientation::Vertical, 0);
            detail.set_border_width(SP as u32);
            paned.pack1(&left, false, false);
            paned.pack2(&detail, true, false);
            root.pack_start(&paned, true, true, 0);
            let footer = status_bar();
            let refresh = gtk::Button::with_label("Refresh");
            let spinner = gtk::Spinner::new();
            let status = gtk::Label::new(None);
            status.set_xalign(0.0);
            status.set_ellipsize(pango::EllipsizeMode::End);
            status.style_context().add_class("dim-label");
            footer.pack_start(&refresh, false, false, 0);
            footer.pack_start(&spinner, false, false, 0);
            footer.pack_start(&status, true, true, 0);
            root.pack_start(&footer, false, false, 0);
            window.add(&root);
            refresh.connect_clicked(glib::clone!(@weak self as app => move |_| app.fetch_index()));
            list.connect_row_selected(glib::clone!(@weak self as app => move |_, row| {
                if app.updating.get() {
                    return;
                }
                app.m.borrow_mut().manager_sel = row.map(|r| r.widget_name().to_string());
                app.refresh_manager_detail();
            }));
            *self.manager.borrow_mut() = Some(Manager { window, list, detail, spinner, status });
        }
        if self.m.borrow().index.is_none() && self.m.borrow().job.is_none() {
            self.fetch_index();
        }
        self.refresh_manager();
        if let Some(pm) = &*self.manager.borrow() {
            pm.window.show_all();
            pm.window.present();
        }
    }

    fn fetch_index(self: &Rc<Self>) {
        if self.m.borrow().job.is_some() {
            return;
        }
        let url = self.m.borrow().psettings.repository().to_string();
        self.start_job("Loading the plugin list…".into(), move || JobResult::Index(plugin::fetch_index(&url)));
    }

    /// Rows of the plugin manager: everything installed or in the repository.
    fn manager_rows(m: &Model) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = m.plugins.iter().map(|p| (p.id().to_string(), p.manifest.name.clone())).collect();
        if let Some(Ok(idx)) = &m.index {
            for e in &idx.plugins {
                if !rows.iter().any(|r| r.0 == e.id) {
                    rows.push((e.id.clone(), e.name.clone()));
                }
            }
        }
        rows
    }

    fn refresh_manager(self: &Rc<Self>) {
        let pm = self.manager.borrow();
        let Some(pm) = &*pm else { return };
        self.updating.set(true);
        for c in pm.list.children() {
            pm.list.remove(&c);
        }
        let (rows, sel, job, index_err) = {
            let mut m = self.m.borrow_mut();
            let rows = Self::manager_rows(&m);
            if m.manager_sel.as_ref().is_none_or(|s| !rows.iter().any(|r| &r.0 == s)) {
                m.manager_sel = rows.first().map(|r| r.0.clone());
            }
            let index_err = match &m.index {
                Some(Err(e)) => Some(e.clone()),
                _ => None,
            };
            (rows, m.manager_sel.clone(), m.job.as_ref().map(|(l, _)| l.clone()), index_err)
        };
        for (id, name) in rows {
            let (row, b) = list_row(&id);
            let l = gtk::Label::new(Some(&name));
            l.set_xalign(0.0);
            b.pack_start(&l, true, true, 0);
            pm.list.add(&row);
            if sel.as_deref() == Some(id.as_str()) {
                pm.list.select_row(Some(&row));
            }
        }
        pm.list.show_all();
        self.updating.set(false);
        match (&job, &index_err) {
            (Some(label), _) => {
                pm.spinner.start();
                pm.status.set_text(label);
            }
            (None, Some(e)) => {
                pm.spinner.stop();
                pm.status.set_text(e);
            }
            (None, None) => {
                pm.spinner.stop();
                pm.status.set_text("Installing needs the administrator password: plugins run inside the kernel.");
            }
        }
        self.refresh_manager_detail();
    }

    fn refresh_manager_detail(self: &Rc<Self>) {
        let pm = self.manager.borrow();
        let Some(pm) = &*pm else { return };
        for c in pm.detail.children() {
            pm.detail.remove(&c);
        }
        let m = self.m.borrow();
        let Some(id) = m.manager_sel.clone() else {
            pm.detail.pack_start(&empty_note("No plugins installed or available."), false, false, 0);
            pm.detail.show_all();
            return;
        };
        let installed = m.plugins.iter().find(|p| p.id() == id);
        let avail = match &m.index {
            Some(Ok(idx)) => idx.plugins.iter().find(|e| e.id == id),
            _ => None,
        };
        let name = installed.map(|p| p.manifest.name.clone()).or_else(|| avail.map(|e| e.name.clone())).unwrap_or(id.clone());
        let desc = installed.map(|p| p.manifest.description.clone()).or_else(|| avail.map(|e| e.description.clone())).unwrap_or_default();
        let license = installed.and_then(|p| p.manifest.license.clone()).or_else(|| avail.and_then(|e| e.license.clone()));
        let homepage = installed.and_then(|p| p.manifest.homepage.clone()).or_else(|| avail.and_then(|e| e.homepage.clone()));

        // Same sections as the Filters tab: header, details, actions.
        let version = installed.map(|p| p.manifest.version.clone()).or_else(|| avail.map(|e| e.version.clone())).unwrap_or_default();
        pm.detail.pack_start(&detail_header(&name, &version, &desc, None), false, false, 0);
        let meta = gtk::Box::new(gtk::Orientation::Vertical, SP_TIGHT);
        meta.set_margin_top(SP_SECTION);
        let mut facts = Vec::new();
        if let Some(p) = installed {
            facts.push(format!("Installed: {}", p.manifest.version));
        }
        if let Some(e) = avail {
            facts.push(format!("Available: {}", e.version));
        }
        if let Some(l) = &license {
            facts.push(format!("License: {l}"));
        }
        let f = gtk::Label::new(Some(&facts.join("  ·  ")));
        f.set_xalign(0.0);
        f.set_ellipsize(pango::EllipsizeMode::End);
        f.style_context().add_class("dim-label");
        meta.pack_start(&f, false, false, 0);
        if let Some(h) = homepage {
            let link = gtk::LinkButton::new(&h);
            link.set_halign(gtk::Align::Start);
            meta.pack_start(&link, false, false, 0);
        }
        pm.detail.pack_start(&meta, false, false, 0);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, SP);
        let busy = m.job.is_some();
        let action = match (installed, avail) {
            (None, Some(_)) => Some("Install"),
            (Some(p), Some(e)) if p.manifest.version != e.version => Some("Update"),
            _ => None,
        };
        if let Some(a) = action {
            let b = gtk::Button::with_label(a);
            b.set_sensitive(!busy);
            let (pid, repo) = (id.clone(), m.psettings.repository().to_string());
            b.connect_clicked(glib::clone!(@weak self as app => move |_| {
                let (pid, repo) = (pid.clone(), repo.clone());
                app.start_job(format!("Installing {pid}…"), move || {
                    JobResult::Done(
                        core::install_plugin(&pid, &repo, false)
                            .map_err(|e| format!("{e}. In a terminal: kerntablet plugin install {pid}")),
                    )
                });
            }));
            buttons.pack_start(&b, false, false, 0);
        }
        if installed.is_some() {
            let b = gtk::Button::with_label("Uninstall");
            b.set_sensitive(!busy);
            let pid = id.clone();
            b.connect_clicked(glib::clone!(@weak self as app => move |_| {
                let pid = pid.clone();
                app.start_job(format!("Removing {pid}…"), move || {
                    JobResult::Done(
                        core::remove_plugin(&pid, false).map_err(|e| format!("{e}. In a terminal: kerntablet plugin remove {pid}")),
                    )
                });
            }));
            buttons.pack_start(&b, false, false, 0);
        }
        pm.detail.pack_end(&buttons, false, false, 0);
        pm.detail.show_all();
    }
}

/// "Wacom CTL-472 (One by Wacom S)" → "Wacom CTL-472", as OTD names tablets.
fn short_name(name: &str) -> String {
    name.split(" (").next().unwrap_or(name).to_string()
}

fn hex(c: &gdk::RGBA) -> String {
    let b = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c.red()), b(c.green()), b(c.blue()))
}

// ---------------------------------------------------------------- geometry

fn full_tablet(spec: &TabletSpec) -> TabletArea {
    TabletArea { width: spec.width_mm, height: spec.height_mm, x: spec.width_mm / 2.0, y: spec.height_mm / 2.0, rotation: 0.0 }
}

fn is_quarter_turn(rotation: f64) -> bool {
    (rotation.rem_euclid(180.0) - 90.0).abs() < 1e-6
}

/// Makes the tablet area's aspect ratio equal to the display area's.
/// `keep_width`: adjust the height (otherwise the width).
fn match_aspect(t: &mut TabletArea, d: &DisplayArea, spec: &TabletSpec, keep_width: bool) {
    if d.width < 1.0 || d.height < 1.0 {
        return;
    }
    let ratio = d.width / d.height;
    if keep_width {
        t.height = t.width / ratio;
    } else {
        t.width = t.height * ratio;
    }
    // Shrink if the result no longer fits on the tablet surface.
    let (bw, bh) = if is_quarter_turn(t.rotation) { (spec.height_mm, spec.width_mm) } else { (spec.width_mm, spec.height_mm) };
    let k = (bw / t.width).min(bh / t.height);
    if k < 1.0 {
        t.width *= k;
        t.height *= k;
    }
    t.width = (t.width * 100.0).round() / 100.0;
    t.height = (t.height * 100.0).round() / 100.0;
}

fn clamp_axis(center: f64, half: f64, total: f64) -> f64 {
    if half * 2.0 <= total {
        center.clamp(half, total - half)
    } else {
        total / 2.0
    }
}

fn clamp_display(d: &mut DisplayArea, desktop: &Desktop) {
    d.x = clamp_axis(d.x, d.width / 2.0, desktop.width);
    d.y = clamp_axis(d.y, d.height / 2.0, desktop.height);
}

fn clamp_tablet(t: &mut TabletArea, spec: &TabletSpec) {
    let (min_x, min_y, max_x, max_y) = transform::area_bounds(t);
    t.x = clamp_axis(t.x, (max_x - min_x) / 2.0, spec.width_mm);
    t.y = clamp_axis(t.y, (max_y - min_y) / 2.0, spec.height_mm);
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: (i32, i32) = (560, 34);
    const T: (i32, i32) = (700, 34);
    const DESK: f64 = 3840.0 / 1080.0;
    const TAB: f64 = 152.0 / 95.0;

    fn lay(w: i32, h: i32) -> OutputLayout {
        output_layout(w, h, DESK, TAB, D, T)
    }

    fn check(l: &OutputLayout, w: i32) {
        let order = [l.display, l.display_row, l.tablet, l.tablet_row];
        for (pair, gap) in order.windows(2).zip([ROW_GAP, BLOCK_GAP, ROW_GAP]) {
            assert_eq!(pair[1].y, pair[0].y + pair[0].h + gap, "{pair:?}");
        }
        assert_eq!(l.display.y, PAD, "anchored at the top");
        for p in order {
            assert!((p.x * 2 + p.w - w).abs() <= 1 || p.x == PAD, "not centred: {p:?}");
        }
        assert!((l.display.w as f64 / l.display.h as f64 - DESK).abs() < 0.1, "{:?}", l.display);
        assert!((l.tablet.w as f64 / l.tablet.h as f64 - TAB).abs() < 0.1, "{:?}", l.tablet);
        assert!((l.tablet.w as f64 - l.display.w as f64 * TABLET_SCALE).abs() <= 1.0 || l.tablet.h == MIN_PREVIEW);
    }

    #[test]
    fn tall_window_uses_full_width() {
        let l = lay(760, 1200);
        check(&l, 760);
        assert_eq!(l.display.w, 760 - 2 * PAD);
    }

    #[test]
    fn short_window_shrinks_both_previews_and_fills_the_height() {
        let l = lay(1200, 520);
        check(&l, 1200);
        let bottom = l.tablet_row.y + l.tablet_row.h;
        assert!((520 - PAD - bottom).abs() <= 2, "unused height: {}", 520 - bottom);
    }

    #[test]
    fn layout_depends_only_on_the_size() {
        assert_eq!(lay(700, 500), lay(700, 500));
        let _ = lay(300, 200);
        assert_eq!(lay(700, 500), lay(700, 500));
    }

    #[test]
    fn tiny_tile_keeps_minimal_previews() {
        let l = lay(200, 100);
        assert!(l.display.h >= MIN_PREVIEW && l.tablet.h >= MIN_PREVIEW);
        assert_eq!(l.display_row.y, l.display.y + l.display.h + ROW_GAP);
    }

    #[test]
    fn millimetres_have_two_or_three_decimals() {
        assert_eq!(mm(58.0), "58.00");
        assert_eq!(mm(39.55), "39.55");
        assert_eq!(mm(75.225), "75.225");
        assert_eq!(mm(12.5), "12.50");
    }
}
