mod appearance_settings_controls;

use std::any::TypeId;

use command_palette_hooks::CommandPaletteFilter;
use editor::EditorSettingsControls;
use feature_flags::{FeatureFlag, FeatureFlagViewExt};
use gpui::{App, Entity, EventEmitter, FocusHandle, Focusable, actions};
use ui::prelude::*;
use workspace::item::{Item, ItemEvent};
use workspace::{Workspace, with_active_or_new_workspace};

use crate::appearance_settings_controls::AppearanceSettingsControls;

pub mod keybindings;
pub mod ui_components;

pub struct SettingsUiFeatureFlag;

impl FeatureFlag for SettingsUiFeatureFlag {
    const NAME: &'static str = "settings-ui";
}

actions!(
    zed,
    [
        /// Opens the settings editor.
        OpenSettingsEditor
    ]
);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenSettingsEditor, cx| {
        with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let existing = workspace
                .active_pane()
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<SettingsPage>());

            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
            } else {
                let settings_page = SettingsPage::new(workspace, cx);
                workspace.add_item_to_active_pane(Box::new(settings_page), None, true, window, cx)
            }
        });
    });

    cx.observe_new(|_workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };

        let settings_ui_actions = [TypeId::of::<OpenSettingsEditor>()];

        CommandPaletteFilter::update_global(cx, |filter, _cx| {
            filter.hide_action_types(&settings_ui_actions);
        });

        cx.observe_flag::<SettingsUiFeatureFlag, _>(
            window,
            move |is_enabled, _workspace, _, cx| {
                if is_enabled {
                    CommandPaletteFilter::update_global(cx, |filter, _cx| {
                        filter.show_action_types(settings_ui_actions.iter());
                    });
                } else {
                    CommandPaletteFilter::update_global(cx, |filter, _cx| {
                        filter.hide_action_types(&settings_ui_actions);
                    });
                }
            },
        )
        .detach();
    })
    .detach();

    keybindings::init(cx);
}

pub struct SettingsPage {
    focus_handle: FocusHandle,
}

impl SettingsPage {
    pub fn new(_workspace: &Workspace, cx: &mut Context<Workspace>) -> Entity<Self> {
        cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
        })
    }
}

impl EventEmitter<ItemEvent> for SettingsPage {}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SettingsPage {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Settings))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Settings".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_4()
            .size_full()
            .gap_4()
            .child(Label::new("Settings").size(LabelSize::Large))
            .child(
                v_flex().gap_1().child(Label::new("Appearance")).child(
                    v_flex()
                        .elevation_2(cx)
                        .child(AppearanceSettingsControls::new()),
                ),
            )
            .child(
                v_flex().gap_1().child(Label::new("Editor")).child(
                    v_flex()
                        .elevation_2(cx)
                        .child(EditorSettingsControls::new()),
                ),
            )
    }
}
