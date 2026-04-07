use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Image, Label, Orientation, Picture, Spinner};
use image::{ImageBuffer, Rgb};
use libadwaita as adw;
use libadwaita::prelude::*;
use qrcode::QrCode;

use crate::bridge::Bridge;

#[derive(Clone)]
pub struct LoginScreen {
    root: Box,
    qr_picture: Picture,
    status_label: Label,
    spinner: Spinner,
    bridge: Arc<Bridge>,
}

impl LoginScreen {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        let root = Box::new(Orientation::Vertical, 24);
        root.set_halign(Align::Center);
        root.set_valign(Align::Center);
        root.set_margin_top(48);
        root.set_margin_bottom(48);

        // App title
        let title = Label::new(Some("WhatsApp"));
        title.add_css_class("title-1");

        let subtitle = Label::new(Some("Connecting…"));
        subtitle.add_css_class("dim-label");

        // QR code display area
        let qr_picture = Picture::new();
        qr_picture.set_size_request(300, 300);
        qr_picture.set_halign(Align::Center);

        // Spinner shown while waiting for QR
        let spinner = Spinner::new();
        spinner.set_spinning(true);
        spinner.set_size_request(48, 48);

        let status_label = Label::new(Some("Loading chats…"));
        status_label.add_css_class("dim-label");

        // QR is hidden by default — only shown when a QR code event arrives
        qr_picture.set_visible(false);

        root.append(&title);
        root.append(&subtitle);
        root.append(&spinner);
        root.append(&qr_picture);
        root.append(&status_label);

        Self {
            root,
            qr_picture,
            status_label,
            spinner,
            bridge,
        }
    }

    pub fn widget(&self) -> &Box {
        &self.root
    }

    pub fn show_qr(&self, qr_string: &str) {
        self.spinner.set_spinning(false);
        self.spinner.set_visible(false);
        // Only show QR instructions when we actually have a QR code
        self.status_label
            .set_text("Scan with WhatsApp on your phone");

        if let Ok(texture) = qr_to_texture(qr_string) {
            self.qr_picture.set_paintable(Some(&texture));
            self.qr_picture.set_visible(true);
        }
    }

    /// Show a loading state (spinner + message), used when already authenticated.
    pub fn show_loading(&self, msg: &str) {
        self.qr_picture.set_visible(false);
        self.spinner.set_spinning(true);
        self.spinner.set_visible(true);
        self.status_label.set_text(msg);
    }

    pub fn show_status(&self, msg: &str) {
        self.status_label.set_text(msg);
        self.qr_picture.set_visible(false);
        self.spinner.set_spinning(true);
        self.spinner.set_visible(true);
    }
}

fn qr_to_texture(content: &str) -> anyhow::Result<gtk4::gdk::MemoryTexture> {
    let code = QrCode::new(content.as_bytes())?;
    let img = code.render::<qrcode::render::unicode::Dense1x2>().build();

    // Render to pixel buffer (white on dark)
    let code2 = QrCode::new(content.as_bytes())?;
    let img_buf = code2
        .render::<image::Luma<u8>>()
        .min_dimensions(300, 300)
        .max_dimensions(300, 300)
        .build();

    let width = img_buf.width() as i32;
    let height = img_buf.height() as i32;

    // Convert grayscale to RGBA
    let rgba: Vec<u8> = img_buf
        .pixels()
        .flat_map(|p| {
            let v = p[0];
            [v, v, v, 255]
        })
        .collect();

    let bytes = glib::Bytes::from(&rgba);
    let texture = gtk4::gdk::MemoryTexture::new(
        width,
        height,
        gtk4::gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        (width * 4) as usize,
    );

    Ok(texture)
}
