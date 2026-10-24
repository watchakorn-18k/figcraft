//! The right-click menu on a layer: "Copy link to selection" puts `figcraft://node/<id>` on the
//! clipboard and says so; with nothing selected the command copies nothing.

use std::sync::{Arc, Mutex};

use egui::Pos2;
use figcraft_engine::Session;
use serde_json::json;

use crate::layers_drag_tests::Ui;
use crate::live_tests::{flat, text_center};
use crate::present_tests::id;

/// A clipboard that keeps what was copied where the test can read it.
struct Copied(Arc<Mutex<Vec<String>>>);

impl crate::Services for Copied {
    fn clipboard_set_text(&mut self, s: &str) {
        if let Ok(mut copied) = self.0.lock() {
            copied.push(s.to_string());
        }
    }
}

#[test]
fn the_context_menu_copies_a_link_to_the_selected_frame() {
    let mut s = Session::new();
    let f = id(&s.execute("frame.create", &json!({"x": 0, "y": 0, "width": 300, "height": 200, "name": "หน้าแรก"})).unwrap());
    let copied = Arc::new(Mutex::new(Vec::new()));
    let mut u = Ui::new(s);
    u.app.services = Box::new(Copied(copied.clone()));
    u.frame(vec![]);
    u.app.ui.context_menu = Some((Pos2::new(700.0, 300.0), vec![f]));
    u.frame(vec![]);
    let shapes = flat(&u.frame_output(vec![]).shapes);
    let at = text_center(&shapes, "Copy link to selection");
    u.button(at, true);
    u.button(at, false);
    assert_eq!(*copied.lock().unwrap(), vec![format!("figcraft://node/{}", f.0)], "the link to the frame");
    assert_eq!(u.app.ui.toast.as_ref().map(|t| t.0.as_str()), Some("Link copied"));
    assert!(u.app.ui.context_menu.is_none(), "the menu closes");
}

#[test]
fn copying_a_link_with_nothing_selected_copies_nothing() {
    let mut s = Session::new();
    s.execute("frame.create", &json!({"x": 0, "y": 0, "width": 300, "height": 200})).unwrap();
    s.execute("select.none", &json!({})).unwrap();
    let copied = Arc::new(Mutex::new(Vec::new()));
    let mut u = Ui::new(s);
    u.app.services = Box::new(Copied(copied.clone()));
    crate::menus::invoke(&mut u.app, "edit.copyLink");
    assert!(copied.lock().unwrap().is_empty(), "no link to node 0");
    assert!(u.app.ui.toast.is_none());
}
