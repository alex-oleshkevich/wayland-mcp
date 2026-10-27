//! GTK4 test fixture for wayland-mcp verification (build with --features fixture).
//! Widgets: editable entry, Increment button + count label, scrollable list,
//! drag surface and an event log label. State changes also print to stdout so
//! app_logs can verify them without any visual inspection.
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Box as GBox, Button, DrawingArea, Entry, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow};
use std::{cell::RefCell, rc::Rc};

fn main() {
    let app = Application::builder().application_id("dev.waylandmcp.fixture").build();
    app.connect_activate(|app| {
        // Disable text-cursor blink: a blinking caret in the entry produces a
        // periodic ~0.25% screen change, which flakes wait_screen(change).
        if let Some(settings) = gtk4::Settings::default() { settings.set_gtk_cursor_blink(false); }
        let window = ApplicationWindow::builder().application(app).title("wayland-mcp fixture").default_width(900).default_height(640).build();
        // Larger text so OCR-based verification is robust at any window size.
        let css = gtk4::CssProvider::new();
        css.load_from_data("window { font-size: 17px; } button { font-size: 18px; } entry { font-size: 17px; }");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let root = GBox::new(Orientation::Vertical, 8);
        root.set_margin_top(12); root.set_margin_bottom(12); root.set_margin_start(12); root.set_margin_end(12);

        let entry = Entry::new();
        entry.set_placeholder_text(Some("type here"));
        root.append(&entry);

        let count = Rc::new(RefCell::new(0i64));
        let count_label = Label::new(Some("Count: 0"));
        let button = Button::with_label("Increment");
        {
            let count = count.clone(); let count_label = count_label.clone();
            button.connect_clicked(move |_| {
                *count.borrow_mut() += 1;
                let text = format!("Count: {}", count.borrow());
                count_label.set_text(&text);
                println!("FIXTURE CLICK count={}", count.borrow());
            });
        }
        root.append(&button);
        root.append(&count_label);

        let event_label = Label::new(Some("Events: none"));
        root.append(&event_label);

        // Scrollable list of 60 rows.
        let list = ListBox::new();
        for i in 0..60 {
            let row = ListBoxRow::new();
            row.set_child(Some(&Label::new(Some(&format!("Row {i}")))));
            list.append(&row);
        }
        let scrolled = ScrolledWindow::builder().vexpand(true).child(&list).build();
        root.append(&scrolled);

        // Drag/touch surface: GtkGestureDrag reports mouse and touch drags.
        let area = DrawingArea::new();
        area.set_content_height(140);
        {
            let event_label1 = event_label.clone();
            let gesture = gtk4::GestureDrag::new();
            gesture.connect_drag_begin(move |_, x, y| {
                let text = format!("Events: drag-begin {x:.0},{y:.0}");
                event_label1.set_text(&text);
                println!("FIXTURE {text}");
            });
            let event_label2 = event_label.clone();
            let gesture2 = gtk4::GestureDrag::new();
            gesture2.connect_drag_end(move |_, x, y| {
                let text = format!("Events: drag-end {x:.0},{y:.0}");
                event_label2.set_text(&text);
                println!("FIXTURE {text}");
            });
            area.add_controller(gesture);
            area.add_controller(gesture2);
        }
        // Pinch/rotate witnesses so multitouch gestures leave a log trace.
        // GestureZoom reports the cumulative scale since gesture begin;
        // GestureRotate reports absolute angle and per-update delta (radians).
        {
            let zoom_label = event_label.clone();
            let zoom = gtk4::GestureZoom::new();
            zoom.connect_scale_changed(move |_, scale| {
                let text = format!("Events: pinch scale={scale:.2}");
                zoom_label.set_text(&text);
                println!("FIXTURE {text}");
            });
            let rotate_label = event_label.clone();
            let rotate = gtk4::GestureRotate::new();
            rotate.connect_angle_changed(move |_, angle, _delta| {
                let text = format!("Events: rotate angle={:.1}", angle.to_degrees());
                rotate_label.set_text(&text);
                println!("FIXTURE {text}");
            });
            area.add_controller(zoom);
            area.add_controller(rotate);
        }
        root.append(&area);

        entry.connect_changed({
            let event_label = event_label.clone();
            move |e| {
                let text = format!("Events: entry={}", e.text());
                event_label.set_text(&text);
                println!("FIXTURE {text}");
            }
        });

        window.set_child(Some(&root));
        window.present();
        println!("FIXTURE READY");
    });
    app.run();
}
