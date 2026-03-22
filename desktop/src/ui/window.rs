use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{glib, Box, Orientation, Paned, Stack};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::{Bridge, WaEvent};
use crate::ui::chat_list::ChatListPanel;
use crate::ui::chat_view::ChatViewPanel;
use crate::ui::login::LoginScreen;

const SIDEBAR_WIDTH: i32 = 360;

#[derive(Clone)]
pub struct MainWindow {
    inner: Rc<MainWindowInner>,
}

struct MainWindowInner {
    window: adw::ApplicationWindow,
    stack: Stack,
    login_screen: LoginScreen,
    chat_list: ChatListPanel,
    chat_view: ChatViewPanel,
    bridge: Arc<Bridge>,
}

impl MainWindow {
    pub fn new(app: &adw::Application, bridge: Arc<Bridge>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("WhatsApp")
            .default_width(1200)
            .default_height(800)
            .build();

        // Top-level stack: login screen vs main chat layout
        let stack = Stack::new();

        // Login screen (QR code)
        let login_screen = LoginScreen::new(bridge.clone());

        // Main layout: sidebar (chat list) + chat view
        let chat_list = ChatListPanel::new(bridge.clone());
        let chat_view = ChatViewPanel::new(bridge.clone());

        let paned = Paned::new(Orientation::Horizontal);
        paned.set_start_child(Some(chat_list.widget()));
        paned.set_end_child(Some(chat_view.widget()));
        paned.set_position(SIDEBAR_WIDTH);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);

        stack.add_named(login_screen.widget(), Some("login"));
        stack.add_named(&paned, Some("main"));
        stack.set_visible_child_name("login");

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.set_content(Some(&stack));

        window.set_content(Some(&toolbar_view));

        let inner = Rc::new(MainWindowInner {
            window,
            stack,
            login_screen,
            chat_list: chat_list.clone(),
            chat_view: chat_view.clone(),
            bridge,
        });

        MainWindow { inner }
    }

    pub fn present(&self) {
        self.inner.window.present();
    }

    pub fn handle_event(&self, event: WaEvent) {
        let inner = &self.inner;
        match event {
            WaEvent::QrCode(qr) => {
                inner.login_screen.show_qr(&qr);
            }
            WaEvent::Connected { phone, name } => {
                log::info!("Connected as {} ({})", name, phone);
                inner.stack.set_visible_child_name("main");
            }
            WaEvent::Disconnected(reason) => {
                log::warn!("Disconnected: {}", reason);
                inner.stack.set_visible_child_name("login");
                inner.login_screen.show_status(&format!("Disconnected: {reason}"));
            }
            WaEvent::ChatsLoaded(chats) => {
                inner.chat_list.load_chats(chats);
            }
            WaEvent::MessageReceived(msg) => {
                inner.chat_view.append_message(msg.clone());
                inner.chat_list.update_last_message(&msg.chat_id, &msg);
            }
            WaEvent::TypingIndicator { chat_id, is_typing } => {
                inner.chat_view.set_typing_indicator(&chat_id, is_typing);
            }
            WaEvent::ReceiptUpdate { msg_id, status } => {
                inner.chat_view.update_receipt(&msg_id, status);
            }
        }
    }
}
