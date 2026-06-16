//! Overlays stacked above the main content area.

use iced::widget::{center, column};
use iced::{Element, Length};

use crate::app::state::DisplayedImage;
use crate::app::{App, Message, Modal, SPINNER_DELAY, TOOLBAR_HEIGHT};
use crate::media::pipeline::Source;
use crate::ui;
use crate::ui::toolbar::LayoutVisibility;

pub(super) fn toolbar_dropdown(app: &App) -> Element<'_, Message> {
    let layout_vis = LayoutVisibility {
        show_filmstrip: app.config.show_filmstrip,
        show_slider: app.config.show_slider,
        show_footer: app.config.show_footer,
        show_info: app.config.show_info,
        show_checkerboard: app.config.show_checkerboard,
    };

    if let Some(dropdown) = ui::toolbar::dropdown(
        app.open_menu,
        app.config.zoom_mode,
        layout_vis,
        app.config.theme == crate::config::ThemeChoice::Light,
        app.config.crisp_pixels,
        app.config.sort_key,
        app.config.sort_desc,
    ) {
        column![dropdown]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        empty()
    }
}

pub(super) fn context_menu(app: &App) -> Element<'_, Message> {
    let Some(pos) = app.context_menu_pos else {
        return empty();
    };
    let toolbar_offset = if app.config.show_toolbar && !app.fullscreen {
        TOOLBAR_HEIGHT
    } else {
        0.0
    };
    let adjusted_pos = iced::Point::new(pos.x, pos.y - toolbar_offset);
    let bounds = iced::Size::new(
        app.window_size.width,
        app.window_size.height - toolbar_offset,
    );
    let clamped =
        ui::context_menu::clamp_menu_pos(adjusted_pos, ui::context_menu::MENU_SIZE, bounds);
    let can_modify =
        !app.config.read_only && app.viewer().is_some_and(|v| matches!(v.source, Source::Fs));
    ui::context_menu::context_menu(clamped, app.config.show_toolbar, can_modify)
}

pub(super) fn spinner(app: &App) -> Element<'_, Message> {
    // Centered spinner only when the viewport has nothing at all to show
    // (the very first load). Once an image is up, the footer's small
    // spinner takes over so nothing covers the picture. With the footer
    // hidden (or fullscreen), a corner spinner keeps progress visible.
    let footer_visible = app.config.show_footer && !app.fullscreen;
    let opening = app
        .opening_since
        .filter(|since| since.elapsed() >= SPINNER_DELAY);
    match app.viewer() {
        // An archive or directory scan is in flight. There may be no
        // viewer yet, so this takes priority over the per-image cases.
        _ if opening.is_some() => {
            let elapsed = opening.map(|since| since.elapsed()).unwrap_or_default();
            center(ui::spinner::spinner(elapsed)).into()
        }
        Some(viewer)
            if matches!(viewer.displayed, DisplayedImage::None)
                && viewer
                    .pending_since
                    .is_some_and(|since| since.elapsed() >= SPINNER_DELAY) =>
        {
            let elapsed = viewer
                .pending_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
            center(ui::spinner::spinner(elapsed)).into()
        }
        Some(viewer)
            if !footer_visible
                && viewer
                    .pending_since
                    .is_some_and(|since| since.elapsed() >= SPINNER_DELAY) =>
        {
            let elapsed = viewer
                .pending_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
            iced::widget::container(ui::spinner::spinner_sized(elapsed, 14.0))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::End)
                .align_y(iced::Alignment::End)
                .padding(12)
                .into()
        }
        _ => empty(),
    }
}

pub(super) fn scrub_bubble(app: &App) -> Element<'_, Message> {
    match app.viewer() {
        Some(viewer) => match viewer.slider_drag {
            Some(drag) if drag.bubble => ui::nav_slider::scrub_bubble(
                viewer.nav.files(),
                drag.target,
                &viewer.thumbs,
                app.window_size,
                app.config.show_footer,
            ),
            _ => empty(),
        },
        None => empty(),
    }
}

pub(super) fn help(app: &App) -> Element<'_, Message> {
    if app.help_open {
        ui::help::help_overlay()
    } else {
        empty()
    }
}

pub(super) fn modal(app: &App) -> Element<'_, Message> {
    match &app.modal {
        Some(Modal::ConfirmDelete(path)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui::dialogs::confirm_delete(&name)
        }
        Some(Modal::Rename { input }) => ui::dialogs::rename_dialog(input),
        Some(Modal::Settings) => ui::settings::settings(
            &app.config,
            app.disk_cache_size,
            app.associations_registered,
        ),
        None => empty(),
    }
}

pub(super) fn toasts(app: &App) -> Element<'_, Message> {
    ui::toast::toast_stack(&app.toasts)
}

fn empty<'a>() -> Element<'a, Message> {
    column![].width(Length::Fill).height(Length::Fill).into()
}
